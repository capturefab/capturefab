//! Faults: camera workers that are killed, stopped or panic, clients that
//! are interrupted or killed, and clients that race to connect a camera.
mod support;

#[cfg(unix)]
use serde_json::Value;
#[cfg(unix)]
use std::time::{Duration, Instant};
#[cfg(unix)]
use std::{path::Path, process::Stdio};
use support::cli::Home;

/// The pid of the worker of session `name`'s only camera.
#[cfg(unix)]
fn worker_pid(home: &Home, name: &str) -> i32 {
    let state = home
        .run_stdin(
            r#"{"op":"status"}"#,
            &["--json", "--session", name, "debug", "rpc"],
        )
        .success();
    state["cameras"][0]["worker_pid"].as_i64().unwrap() as i32
}

/// Files in `dir`, sorted.
#[cfg(unix)]
fn files_in(dir: &Path) -> Vec<String> {
    let mut files = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.path().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    files.sort();
    files
}

/// Wait until `dir` holds at least `count` files.
#[cfg(unix)]
fn wait_for_files(dir: &Path, count: usize) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while files_in(dir).len() < count {
        assert!(Instant::now() < deadline, "no frames saved in {dir:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The document a finished child printed on stdout.
#[cfg(unix)]
fn document(output: &std::process::Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

#[cfg(unix)]
fn send(pid: i32, signal: i32) {
    // SAFETY: kill only sends a signal to a process this test started.
    assert_eq!(unsafe { libc::kill(pid, signal) }, 0);
}

/// Whether any process is left in process group `group`.
#[cfg(unix)]
fn group_lives(group: i32) -> bool {
    // SAFETY: signal 0 only checks whether the group has members.
    unsafe { libc::kill(-group, 0) == 0 }
}

/// A worker killed under a blocking capture fails that capture as
/// worker_crashed; the next command on the camera starts another worker.
#[cfg(unix)]
#[test]
fn a_killed_worker_fails_the_command_in_flight_and_the_next_reconnects() {
    let home = Home::new("fault-kill");
    let _session = home.serve("crash", &["sim:0"]);
    let worker = worker_pid(&home, "crash");
    let child = home
        .command()
        .args([
            "--json", "capture", "-c", "sim:0", "--every", "100ms", "-n", "1000", "-o", "lapse/",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Killed once the session has heard of two files: a file published
    // in the instant before a kill may not be.
    let deadline = Instant::now() + Duration::from_secs(20);
    while home.ok(&["jobs"])["jobs"][0]["saved"].as_u64().unwrap_or(0) < 2 {
        assert!(Instant::now() < deadline, "no frames saved");
        std::thread::sleep(Duration::from_millis(50));
    }
    send(worker, libc::SIGKILL);
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(3), "{}", document(&output));
    let error = &document(&output)["error"];
    assert_eq!(error["code"], "worker_crashed", "{error}");
    assert_eq!(error["details"]["signal"], 9, "{error}");
    assert_eq!(error["details"]["job"]["state"], "failed", "{error}");
    // What it saved before stays listed.
    assert!(error["details"]["files"].as_array().unwrap().len() >= 2);
    assert!(!support::cli::process_lives(worker));
    // The next command reconnects the camera.
    assert_eq!(
        home.ok(&["get", "-c", "sim:0", "Width"])["values"]["Width"],
        640
    );
    assert_ne!(worker_pid(&home, "crash"), worker);
    let status = home.ok(&["session", "status", "crash"]);
    assert_eq!(status["cameras"][0]["state"], "idle", "{status}");
}

/// A worker that stops answering, here stopped by SIGSTOP, is ended within
/// its hang limit: the command times out, and the worker and its process
/// group are gone.
#[cfg(unix)]
#[test]
fn a_hung_worker_is_ended_and_its_command_times_out() {
    let home = Home::new("fault-hang");
    let _session = home.serve("hang", &["sim:0"]);
    let worker = worker_pid(&home, "hang");
    let child = home
        .command()
        .args([
            "--json", "capture", "-c", "sim:0", "--every", "100ms", "-n", "1000", "-o", "lapse/",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for_files(&home.path("lapse"), 2);
    let stopped = Instant::now();
    send(worker, libc::SIGSTOP);
    let output = child.wait_with_output().unwrap();
    let took = stopped.elapsed();
    assert_eq!(output.status.code(), Some(4), "{}", document(&output));
    assert_eq!(document(&output)["error"]["code"], "timeout");
    // Ten seconds of silence, and a moment to notice it.
    assert!(took < Duration::from_secs(20), "{took:?}");
    assert!(!support::cli::process_lives(worker), "reaped");
    assert!(!group_lives(worker), "nothing left in its group");
    assert_eq!(
        home.ok(&["get", "-c", "sim:0", "Width"])["values"]["Width"],
        640
    );
}

/// SIGINT during a burst ends it with exit 130, and lists exactly the
/// files that were published: nothing half-written is left beside them.
#[cfg(unix)]
#[test]
fn an_interrupted_burst_lists_exactly_the_files_it_published() {
    let home = Home::new("fault-burst");
    let mut child = home
        .command()
        .args([
            "--json", "capture", "-c", "sim:0", "-n", "1000", "-o", "burst/",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for_files(&home.path("burst"), 3);
    support::cli::signal_and_wait(&mut child, libc::SIGINT);
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(130));
    let error = &document(&output)["error"];
    assert_eq!(error["code"], "interrupted", "{error}");
    let mut listed = error["details"]["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|file| file.as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    listed.sort();
    let on_disk = files_in(&std::fs::canonicalize(home.path("burst")).unwrap());
    assert_eq!(listed, on_disk);
    assert!(listed.len() >= 3 && listed.len() < 1000);
    assert_eq!(error["details"]["job"]["state"], "cancelled");
}

/// SIGINT to a client recording through a session ends the recording
/// normally, its file complete.
#[cfg(unix)]
#[test]
#[ignore = "requires FFmpeg; set CAPTUREFAB_FFMPEG to the executable"]
fn a_signal_ends_a_session_recording_normally() {
    let home = Home::new("fault-record");
    let _session = home.serve("rec", &["sim:rec"]);
    home.ok(&["set", "-c", "sim:rec", "Width=64", "Height=64"]);
    let mut child = home
        .command()
        .args([
            "--json", "record", "-c", "sim:rec", "-o", "clip.mkv", "--fps", "10",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::fs::metadata(home.path("clip.mkv")).map_or(0, |meta| meta.len()) == 0 {
        assert!(Instant::now() < deadline, "the recording did not start");
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_secs(1));
    support::cli::signal_and_wait(&mut child, libc::SIGINT);
    let output = child.wait_with_output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result = document(&output);
    assert_eq!(result["ended"], "signal", "{result}");
    assert_eq!(result["finalized"], true, "{result}");
    assert!(support::cli::decodes(&home.path("clip.mkv")));
    let jobs = home.ok(&["jobs"]);
    assert_eq!(jobs["jobs"][0]["state"], "done", "{jobs}");
}

/// A client killed while it waits for a session capture leaves no job
/// running: with no wait for ten seconds, the session stops it.
#[cfg(unix)]
#[test]
fn a_killed_client_leaves_no_job_running() {
    let home = Home::new("fault-lease");
    let _session = home.serve("lease", &["sim:0"]);
    let mut child = home
        .command()
        .args([
            "--json", "capture", "-c", "sim:0", "--every", "200ms", "-n", "1000", "-o", "lapse/",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_for_files(&home.path("lapse"), 2);
    child.kill().unwrap();
    child.wait().unwrap();
    let killed = Instant::now();
    let deadline = killed + Duration::from_secs(20);
    loop {
        let jobs = home.ok(&["jobs"]);
        let job = &jobs["jobs"][0];
        if job["state"] == "cancelled" {
            break;
        }
        assert!(Instant::now() < deadline, "the job ran on: {job}");
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(
        killed.elapsed() >= Duration::from_secs(5),
        "not before the lease"
    );
    let saved = files_in(&home.path("lapse")).len();
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(files_in(&home.path("lapse")).len(), saved, "no more frames");
}

/// Clients connecting the same camera at once in one session share one
/// worker.
#[test]
fn concurrent_connects_to_one_camera_share_a_worker() {
    let home = Home::new("fault-race");
    let _session = home.serve("race", &[]);
    for round in 0..3 {
        let camera = format!("sim:race{round}");
        let results = std::thread::scope(|scope| {
            let threads = (0..4)
                .map(|_| {
                    let camera = camera.clone();
                    let home = &home;
                    scope.spawn(move || {
                        home.run(&["--json", "--session", "race", "get", "-c", &camera, "Width"])
                    })
                })
                .collect::<Vec<_>>();
            threads
                .into_iter()
                .map(|thread| thread.join().unwrap())
                .collect::<Vec<_>>()
        });
        for run in results {
            assert_eq!(run.success()["values"]["Width"], 640);
        }
        let status = home.ok(&["session", "status", "race"]);
        let held = status["cameras"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|held| held["id"] == camera.as_str())
            .count();
        assert_eq!(held, 1, "{status}");
        // One worker per camera runs, and no other was left behind.
        #[cfg(unix)]
        {
            let children = std::process::Command::new("pgrep")
                .args(["-P", &_session.child.id().to_string()])
                .output()
                .unwrap();
            let workers = String::from_utf8_lossy(&children.stdout).lines().count();
            assert_eq!(workers, round + 1, "{status}");
        }
    }
}

/// A worker that panics is reported as crashed, with what it printed; a
/// camera whose worker keeps crashing stays in error until connected again.
#[cfg(unix)]
#[test]
fn a_panicking_worker_is_reported_with_its_last_words() {
    let home = Home::new("fault-panic");
    let crash = |args: &[&str]| {
        let run = home.run_env(
            &[("CAPTUREFAB_TEST_HOOKS", "1")],
            &[&["--json"][..], args].concat(),
        );
        run.failure(3)
    };
    let direct = crash(&[
        "capture",
        "-c",
        "sim:0?panic=after:3",
        "-n",
        "10",
        "-o",
        "direct/",
    ]);
    assert_eq!(direct["code"], "worker_crashed", "{direct}");
    let tail = direct["details"]["stderr_tail"].as_array().unwrap();
    assert!(
        tail.iter()
            .any(|line| line.as_str().unwrap().contains("simulated camera fault")),
        "{direct}"
    );
    // A session's worker that keeps crashing: three times, then the camera
    // stays in error.
    let mut command = home.command();
    command.env("CAPTUREFAB_TEST_HOOKS", "1");
    let _session = home.serve_in(&mut command, "panic", &["sim:0?panic=after:3"], &[]);
    for _ in 0..3 {
        let crashed = crash(&[
            "capture",
            "-c",
            "sim:0?panic=after:3",
            "-n",
            "10",
            "-o",
            "s/",
        ]);
        assert_eq!(crashed["code"], "worker_crashed", "{crashed}");
    }
    let started = Instant::now();
    let looping = crash(&["get", "-c", "sim:0?panic=after:3", "Width"]);
    assert_eq!(looping["code"], "worker_crashed", "{looping}");
    assert!(
        looping["message"].as_str().unwrap().contains("3 times"),
        "{looping}"
    );
    assert!(started.elapsed() < Duration::from_secs(5));
    let camera = &home.ok(&["session", "status", "panic"])["cameras"][0];
    assert_eq!(camera["state"], "error", "{camera}");
    let error = &camera["error"];
    assert_eq!(error["code"], "worker_crashed", "{camera}");
    assert!(
        error["details"]["stderr_tail"]
            .as_array()
            .unwrap()
            .iter()
            .any(|line| line.as_str().unwrap().contains("simulated camera fault")),
        "{camera}"
    );
}

/// A session told to stop while a camera connects stops all the same: the
/// camera that connects as the shutdown runs is refused and its worker
/// ended, not kept for no one. Here the shutdown runs a while because
/// another camera's worker was stopped.
#[cfg(unix)]
#[test]
fn a_session_stops_while_a_camera_connects() {
    let home = Home::new("fault-late");
    let mut command = home.command();
    command.env("CAPTUREFAB_TEST_HOOKS", "1");
    let mut session = home.serve_in(&mut command, "late", &["sim:1"], &[]);
    let stuck = worker_pid(&home, "late");
    let connecting = home
        .command()
        .env("CAPTUREFAB_TEST_HOOKS", "1")
        .args([
            "--json",
            "--session",
            "late",
            "get",
            "-c",
            "sim:0?connect=after:2000",
            "Width",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(700));
    send(stuck, libc::SIGSTOP);
    send(session.child.id() as i32, libc::SIGTERM);
    let output = connecting.wait_with_output().unwrap();
    let error = &document(&output)["error"];
    assert_eq!(output.status.code(), Some(3), "{error}");
    assert!(
        error["message"].as_str().unwrap().contains("shutting down"),
        "{error}"
    );
    // Only the stopped worker is left while the shutdown waits for it.
    let children = std::process::Command::new("pgrep")
        .args(["-P", &session.child.id().to_string()])
        .output()
        .unwrap();
    let left = String::from_utf8_lossy(&children.stdout)
        .lines()
        .map(|pid| pid.trim().parse::<i32>().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(left, [stuck]);
    let deadline = Instant::now() + Duration::from_secs(30);
    while session.child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "the session did not stop");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!support::cli::process_lives(stuck));
}

/// A client killed while it waits for a capture that saves nothing, as on
/// a camera waiting for a trigger, is noticed when it goes: the job is
/// stopped once its lease runs out, ten seconds later.
#[cfg(unix)]
#[test]
fn a_killed_client_of_a_quiet_capture_is_noticed_when_it_goes() {
    let home = Home::new("fault-quiet");
    let _session = home.serve("quiet", &["sim:0"]);
    home.ok(&[
        "set",
        "-c",
        "sim:0",
        "TriggerMode=On",
        "TriggerSource=Line0",
    ]);
    let mut child = home
        .command()
        .args([
            "--json",
            "capture",
            "-c",
            "sim:0",
            "-o",
            "shot.png",
            "--timeout",
            "none",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !matches!(
        home.ok(&["jobs"])["jobs"][0]["state"].as_str(),
        Some("armed" | "running")
    ) {
        assert!(Instant::now() < deadline, "no job started");
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(300));
    child.kill().unwrap();
    child.wait().unwrap();
    let killed = Instant::now();
    loop {
        let jobs = home.ok(&["jobs"]);
        if jobs["jobs"][0]["state"] == "cancelled" {
            break;
        }
        assert!(
            killed.elapsed() < Duration::from_secs(16),
            "the job ran on: {jobs}"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(
        killed.elapsed() >= Duration::from_secs(5),
        "not before the lease"
    );
}

/// SIGINT to a capture to stdout through a session ends it at once, and the
/// camera is free for the session's next command.
#[cfg(unix)]
#[test]
fn an_interrupted_session_capture_to_stdout_frees_the_camera() {
    let home = Home::new("fault-stdout");
    let _session = home.serve("held", &["sim:0"]);
    home.ok(&[
        "set",
        "-c",
        "sim:0",
        "TriggerMode=On",
        "TriggerSource=Line0",
    ]);
    let image = std::fs::File::create(home.path("image")).unwrap();
    let mut child = home
        .command()
        .args([
            "--json",
            "capture",
            "-c",
            "sim:0",
            "-o",
            "-",
            "--timeout",
            "none",
        ])
        .stdout(image)
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    let asked = Instant::now();
    let status = support::cli::signal_and_wait(&mut child, libc::SIGINT);
    assert_eq!(status.code(), Some(130));
    assert!(asked.elapsed() < Duration::from_secs(3));
    let asked = Instant::now();
    assert_eq!(
        home.ok(&["get", "-c", "sim:0", "Width"])["values"]["Width"],
        640
    );
    assert!(asked.elapsed() < Duration::from_secs(5));
    let out = home.sessions.join("out");
    let left = std::fs::read_dir(&out)
        .map(|entries| entries.count())
        .unwrap_or(0);
    assert_eq!(left, 0, "the private file is deleted");
}

/// A second signal ends the camera workers of the command, and what they
/// started, with it: they lead process groups of their own, which the
/// terminal's signal never reaches. Here the worker is opening its camera,
/// which takes seconds, and would finish that before it exits.
#[cfg(unix)]
#[test]
fn a_second_signal_ends_the_workers_at_once() {
    let home = Home::new("fault-twice");
    let mut child = home
        .command()
        .env("CAPTUREFAB_TEST_HOOKS", "1")
        .args([
            "--json",
            "capture",
            "-c",
            "sim:0?connect=after:8000",
            "-o",
            "shot.png",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let worker = loop {
        let children = std::process::Command::new("pgrep")
            .args(["-P", &child.id().to_string()])
            .output()
            .unwrap();
        if let Some(pid) = String::from_utf8_lossy(&children.stdout).lines().next() {
            break pid.trim().parse::<i32>().unwrap();
        }
        assert!(Instant::now() < deadline, "no worker started");
        std::thread::sleep(Duration::from_millis(20));
    };
    std::thread::sleep(Duration::from_millis(500));
    send(child.id() as i32, libc::SIGINT);
    std::thread::sleep(Duration::from_millis(200));
    let status = support::cli::signal_and_wait(&mut child, libc::SIGINT);
    assert_eq!(status.code(), Some(130));
    let ended = Instant::now();
    while group_lives(worker) {
        assert!(
            ended.elapsed() < Duration::from_secs(2),
            "the worker's group outlived the command"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A blocking capture on more cameras than a session holds long waits for
/// saves every frame of every camera: the waits past that limit are short.
#[cfg(unix)]
#[test]
fn a_capture_on_more_cameras_than_long_waits_saves_every_frame() {
    let home = Home::new("fault-many");
    let cameras = (0..13)
        .map(|index| format!("sim:{index}"))
        .collect::<Vec<_>>();
    let selectors = cameras.iter().map(String::as_str).collect::<Vec<_>>();
    let _session = home.serve("many", &selectors);
    let mut args = vec!["--json", "capture"];
    for camera in &selectors {
        args.extend(["-c", camera]);
    }
    args.extend(["-n", "2", "--every", "2s", "-o", "shots/{camera}-{seq}.png"]);
    let run = home.run(&args);
    let result = run.success();
    assert_eq!(result["cameras"].as_array().unwrap().len(), 13, "{result}");
    assert_eq!(files_in(&home.path("shots")).len(), 26);
}

/// The jobs of a session that died read as lost, with what they saved.
#[cfg(unix)]
#[test]
fn a_dead_sessions_jobs_read_as_lost() {
    let home = Home::new("fault-lost");
    let started = home.ok(&["session", "start", "--name", "doomed", "-c", "sim:0"]);
    let pid = started["pid"].as_i64().unwrap() as i32;
    let job = home.ok(&[
        "capture",
        "-c",
        "sim:0",
        "--every",
        "100ms",
        "-n",
        "1000",
        "-o",
        "lapse/",
        "--background",
    ]);
    let id = job["job"]["id"].as_str().unwrap().to_owned();
    // Saved counts reach the job book's file within a second.
    let deadline = Instant::now() + Duration::from_secs(20);
    while home.ok(&["jobs"])["jobs"][0]["saved"].as_u64().unwrap_or(0) < 3 {
        assert!(Instant::now() < deadline, "no frames saved");
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(1200));
    send(pid, libc::SIGKILL);
    assert!(!support::cli::process_lives(pid));
    let jobs = home.ok(&["jobs"]);
    let lost = &jobs["jobs"][0];
    assert_eq!(lost["id"], id.as_str(), "{jobs}");
    assert_eq!(lost["state"], "lost", "{jobs}");
    assert!(lost["saved"].as_u64().unwrap() >= 3, "{jobs}");
    let waited = home.fails(1, &["jobs", "wait", &id]);
    assert_eq!(waited["code"], "incomplete");
    assert_eq!(waited["details"]["job"]["state"], "lost");
    // A session of the same name goes on numbering after them.
    home.ok(&["session", "start", "--name", "doomed", "-c", "sim:0"]);
    let next = home.ok(&[
        "--session",
        "doomed",
        "capture",
        "-o",
        "next/",
        "--background",
    ]);
    assert_ne!(next["job"]["id"], id.as_str());
    home.ok(&["session", "stop", "doomed"]);
}
