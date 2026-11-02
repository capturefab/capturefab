//! Sessions: started in the background or served in the foreground, found
//! by the cameras they hold, kept by --keep, running --background jobs, and
//! ended by `session stop`, idle time or signals.
mod support;

use serde_json::json;
use std::time::{Duration, Instant};
use support::cli::Home;

/// A started session gets its cameras on stdin: an address can hold a
/// password, and a process's arguments are for every user to read.
#[cfg(unix)]
#[test]
fn a_started_session_keeps_its_cameras_out_of_its_arguments() {
    let home = Home::new("quiet-args");
    let started = home.ok(&[
        "session",
        "start",
        "--name",
        "quiet",
        "-c",
        "sim:3?drop=0.0001",
        "--idle",
        "1m",
    ]);
    assert_eq!(
        started["cameras"],
        json!(["sim:3?drop=0.0001"]),
        "{started}"
    );
    let pid = started["pid"].as_i64().unwrap().to_string();
    let args = std::process::Command::new("ps")
        .args(["-o", "args=", "-p", &pid])
        .output()
        .unwrap();
    let args = String::from_utf8_lossy(&args.stdout);
    assert!(
        args.contains("serve") && args.contains("--cameras-stdin"),
        "{args}"
    );
    assert!(!args.contains("sim:3"), "{args}");
    home.ok(&["session", "stop", "quiet"]);
}

#[test]
fn a_session_starts_answers_and_stops() {
    let home = Home::new("lifecycle");
    let started = home.ok(&[
        "session", "start", "--name", "bench", "-c", "sim:0", "--idle", "1m",
    ]);
    assert_eq!(started["session"], "bench");
    assert_eq!(started["cameras"], json!(["sim:0"]));
    assert_eq!(started["started"], true);
    let pid = started["pid"].as_i64().unwrap();
    // Starting it again keeps it, and adds a camera asked for.
    let again = home.ok(&["session", "start", "--name", "bench", "-c", "sim:1"]);
    assert_eq!(again["started"], false);
    assert_eq!(again["pid"], pid);
    assert_eq!(again["cameras"], json!(["sim:0", "sim:1"]));
    // --live on a running session starts the cameras it names streaming.
    home.ok(&[
        "session", "start", "--name", "bench", "-c", "sim:2", "--live",
    ]);
    let states = home.ok(&["session", "status", "bench"])["cameras"]
        .as_array()
        .unwrap()
        .iter()
        .map(|camera| (camera["id"].clone(), camera["state"].clone()))
        .collect::<Vec<_>>();
    assert_eq!(
        states,
        [
            (json!("sim:0"), json!("idle")),
            (json!("sim:1"), json!("idle")),
            (json!("sim:2"), json!("streaming"))
        ]
    );
    let list = home.ok(&["session", "list"]);
    assert_eq!(list["sessions"][0]["session"], "bench");
    let status = home.ok(&["session", "status", "bench"]);
    assert_eq!(status["pid"], pid);
    assert_eq!(status["cameras"].as_array().unwrap().len(), 3);
    // Commands find it by the camera it holds; its settings persist.
    home.ok(&["set", "-c", "sim:1", "Width=320"]);
    assert_eq!(
        home.ok(&["get", "-c", "sim:1", "Width"])["values"]["Width"],
        320
    );
    let stopped = home.ok(&["session", "stop", "bench"]);
    assert_eq!(stopped["stopped"], true);
    #[cfg(unix)]
    assert!(!support::cli::process_lives(pid as i32));
    assert!(
        home.ok(&["session", "list"])["sessions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let gone = home.fails(3, &["session", "stop", "bench"]);
    assert_eq!(gone["code"], "session_not_found");
    // A camera that cannot connect stops the session before it is ready.
    let failed = home.fails(3, &["session", "start", "--name", "broken", "-c", "sim9"]);
    assert_eq!(failed["code"], "camera_not_found");
    assert!(
        home.ok(&["session", "list"])["sessions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn an_idle_session_ends_on_its_own() {
    let home = Home::new("idle");
    let started = home.ok(&[
        "session", "start", "--name", "brief", "-c", "sim:0", "--idle", "1s",
    ]);
    let pid = started["pid"].as_i64().unwrap();
    // Looking at it does not keep it.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let list = home.ok(&["session", "list"]);
        if list["sessions"].as_array().unwrap().is_empty() {
            break;
        }
        assert!(Instant::now() < deadline, "the session stayed: {list}");
        std::thread::sleep(Duration::from_millis(200));
    }
    #[cfg(unix)]
    assert!(!support::cli::process_lives(pid as i32));
    let _ = pid;
}

#[test]
fn keep_starts_a_session_once_and_reuses_it() {
    let home = Home::new("keep");
    let first_started = Instant::now();
    let first = home.ok(&["get", "-c", "sim:0", "Width", "--keep", "1m"]);
    let first_took = first_started.elapsed();
    let sessions = home.ok(&["session", "list"]);
    assert_eq!(sessions["sessions"][0]["session"], "auto");
    assert_eq!(sessions["sessions"][0]["cameras"], json!(["sim:0"]));
    let second_started = Instant::now();
    let second = home.ok(&["get", "-c", "sim:0", "Width", "--keep", "1m"]);
    assert!(
        second_started.elapsed() < first_took,
        "{:?} then {:?}",
        first_took,
        second_started.elapsed()
    );
    assert_eq!(first, second);
    // CAPTUREFAB_KEEP does the same; one session, still.
    let run = home.run_env(
        &[("CAPTUREFAB_KEEP", "1m")],
        &["--json", "info", "-c", "sim:0"],
    );
    run.success();
    assert_eq!(
        home.ok(&["session", "list"])["sessions"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    // A session holds its settings between calls.
    home.ok(&["set", "-c", "sim:0", "Width=320"]);
    assert_eq!(
        home.ok(&["get", "-c", "sim:0", "Width"])["values"]["Width"],
        320
    );
    let refused = home.fails(
        2,
        &[
            "--no-session",
            "get",
            "-c",
            "sim:0",
            "Width",
            "--keep",
            "1m",
        ],
    );
    assert_eq!(refused["code"], "usage");
}

#[test]
fn background_jobs_run_on_and_can_be_waited_for_or_stopped() {
    let home = Home::new("background");
    let started = home.ok(&[
        "capture",
        "-c",
        "sim:0",
        "--every",
        "200ms",
        "-n",
        "3",
        "-o",
        "lapse/",
        "--background",
    ]);
    let job = &started["job"];
    assert_eq!(job["id"], "auto-1");
    assert_eq!(job["kind"], "capture");
    assert_eq!(job["planned"], 3);
    assert!(
        matches!(job["state"].as_str(), Some("armed" | "running")),
        "{job}"
    );
    let listed = home.ok(&["jobs"]);
    assert_eq!(listed["jobs"][0]["id"], "auto-1");
    let done = home.ok(&["jobs", "wait", "auto-1", "--timeout", "20s"]);
    assert_eq!(done["job"]["state"], "done");
    assert_eq!(done["job"]["saved"], 3);
    assert_eq!(std::fs::read_dir(home.path("lapse")).unwrap().count(), 3);
    // A long time lapse is stopped; the files so far stay.
    let long = home.ok(&[
        "capture",
        "-c",
        "sim:0",
        "--every",
        "10s",
        "-n",
        "100",
        "-o",
        "long/",
        "--background",
    ]);
    let id = long["job"]["id"].as_str().unwrap().to_owned();
    assert_eq!(id, "auto-2");
    let still = home.fails(4, &["jobs", "wait", &id, "--timeout", "100ms"]);
    assert_eq!(still["code"], "timeout");
    assert_eq!(still["details"]["job"]["id"], id.as_str());
    let stopped = home.ok(&["jobs", "stop", &id]);
    assert_eq!(stopped["stopped"][0]["state"], "cancelled");
    let cancelled = home.fails(1, &["jobs", "wait", &id]);
    assert_eq!(cancelled["code"], "incomplete");
    assert_eq!(cancelled["details"]["job"]["state"], "cancelled");
    // Jobs are numbered across a session's cameras.
    let both = home.ok(&[
        "capture",
        "-c",
        "sim:0",
        "-c",
        "sim:1",
        "-o",
        "both/{camera}-{seq}.png",
        "--background",
    ]);
    let ids = both["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|job| job["id"].as_str().unwrap().to_owned())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(ids.len(), 2, "{both}");
    for id in &ids {
        assert_eq!(home.ok(&["jobs", "wait", id])["job"]["saved"], 1);
    }
    // A job that cannot take its first frame fails the command that
    // started it, with its own reason; it stays listed as failed.
    home.ok(&[
        "set",
        "-c",
        "sim:0",
        "TriggerMode=On",
        "TriggerSource=Line1",
    ]);
    let triggered = home.fails(
        4,
        &[
            "capture",
            "-c",
            "sim:0",
            "--timeout",
            "200ms",
            "-o",
            "never.png",
            "--background",
        ],
    );
    assert_eq!(triggered["code"], "trigger_timeout");
    let id = triggered["details"]["job"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(triggered["details"]["job"]["state"], "failed");
    let failed = home.fails(4, &["jobs", "wait", &id, "--timeout", "10s"]);
    assert_eq!(failed["code"], "trigger_timeout");
    assert_eq!(failed["details"]["job"]["error"]["code"], "trigger_timeout");
    // Stopping it shows it as it ended, with its error.
    let stopped = home.ok(&["jobs", "stop", &id]);
    assert_eq!(stopped["stopped"][0]["error"]["code"], "trigger_timeout");
}

#[cfg(unix)]
#[test]
fn serve_shuts_down_cleanly_on_sigterm_and_sighup() {
    for (signal, name) in [(libc::SIGTERM, "term"), (libc::SIGHUP, "hup")] {
        let home = Home::new(&format!("signal-{name}"));
        let mut serve = home.serve_with(name, &["sim:0"], &["--live"]);
        let state = home.run_stdin(
            r#"{"op":"status"}"#,
            &["--json", "--session", name, "debug", "rpc"],
        );
        let state = state.success();
        let worker = state["cameras"][0]["worker_pid"].as_i64().unwrap() as i32;
        assert!(support::cli::process_lives(worker));
        // Camera workers have their own process groups, outside terminal signals.
        assert_eq!(unsafe { libc::getpgid(worker) }, worker);
        let shot = home.ok(&["capture", "-o", "saved.png"]);
        let saved = std::fs::read(shot["files"][0].as_str().unwrap()).unwrap();
        assert!(saved.starts_with(b"\x89PNG"));
        let descriptor = home.sessions.join(format!("{name}.json"));
        assert!(descriptor.is_file());
        let exit = serve.signal(signal);
        assert!(exit.success(), "{name}: {exit:?}");
        assert!(!descriptor.exists(), "{name}: the descriptor is removed");
        assert!(
            !support::cli::process_lives(worker),
            "{name}: the camera worker exits"
        );
        assert_eq!(std::fs::read(home.path("saved.png")).unwrap(), saved);
    }
}

/// `nohup` starts serve with SIGHUP ignored, so it must outlive the
/// terminal it was started from; SIGTERM still stops it.
#[cfg(unix)]
#[test]
fn serve_under_nohup_ignores_sighup() {
    use std::os::unix::process::CommandExt;
    let home = Home::new("nohup");
    let mut command = home.command();
    // SAFETY: signal is async-signal-safe, as pre_exec requires.
    unsafe {
        command.pre_exec(|| {
            libc::signal(libc::SIGHUP, libc::SIG_IGN);
            Ok(())
        });
    }
    let mut serve = home.serve_in(&mut command, "nohup", &[], &[]);
    // SAFETY: kill only sends a signal to the child this test spawned.
    assert_eq!(
        unsafe { libc::kill(serve.child.id() as i32, libc::SIGHUP) },
        0
    );
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        serve.child.try_wait().unwrap().is_none(),
        "serve outlives SIGHUP"
    );
    assert!(home.sessions.join("nohup.json").is_file());
    let exit = serve.signal(libc::SIGTERM);
    assert!(exit.success(), "{exit:?}");
}

#[test]
fn routing_follows_the_cameras_sessions_hold() {
    let home = Home::new("routing-held");
    let _first = home.serve("first", &["sim:first"]);
    let _second = home.serve("second", &["sim:a", "sim:b"]);
    // The session that holds the camera answers.
    home.ok(&["set", "-c", "sim:b", "Width=320"]);
    assert_eq!(
        home.ok(&["get", "-c", "sim:b", "Width"])["values"]["Width"],
        320
    );
    assert_eq!(
        home.ok(&["--session", "second", "get", "-c", "sim:b", "Width"])["values"]["Width"],
        320
    );
    // A camera no session holds gets a worker of its own.
    assert_eq!(
        home.ok(&["get", "-c", "sim:c", "Width"])["values"]["Width"],
        640
    );
    // Without -c: the only session holding exactly one camera.
    assert_eq!(home.ok(&["get", "Width"])["camera"], "sim:first");
    // A session lists the cameras it holds, connected later too.
    home.ok(&["--session", "first", "get", "-c", "sim:late", "Width"]);
    let descriptor: serde_json::Value =
        serde_json::from_slice(&std::fs::read(home.sessions.join("first.json")).unwrap()).unwrap();
    let held = descriptor["cameras"]
        .as_array()
        .unwrap()
        .iter()
        .map(|camera| camera["id"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(held, ["sim:first", "sim:late"]);
    assert_eq!(
        home.ok(&["set", "-c", "sim:late", "Width=256"])["changed"][0]["to"],
        256
    );
    assert_eq!(
        home.ok(&["--session", "first", "get", "-c", "sim:late", "Width"])["values"]["Width"],
        256
    );
}

/// A camera worker that dies fails its jobs, so nothing waits for ever.
#[cfg(unix)]
#[test]
fn a_dead_worker_fails_its_jobs() {
    let home = Home::new("dead-worker");
    let _session = home.serve("dead", &["sim:0"]);
    let job = home.ok(&[
        "capture",
        "-c",
        "sim:0",
        "--every",
        "1s",
        "-n",
        "100",
        "-o",
        "lapse/",
        "--background",
    ]);
    let id = job["job"]["id"].as_str().unwrap().to_owned();
    let state = home
        .run_stdin(
            r#"{"op":"status"}"#,
            &["--json", "--session", "dead", "debug", "rpc"],
        )
        .success();
    let worker = state["cameras"][0]["worker_pid"].as_i64().unwrap() as i32;
    // SAFETY: kill only sends a signal to the worker this test's session runs.
    assert_eq!(unsafe { libc::kill(worker, libc::SIGKILL) }, 0);
    let failed = home.fails(3, &["jobs", "wait", &id, "--timeout", "10s"]);
    assert_eq!(failed["code"], "worker_crashed");
    assert_eq!(failed["details"]["job"]["state"], "failed");
    // The camera is in error until the session stops.
    let camera = &home.ok(&["session", "status", "dead"])["cameras"][0];
    assert_eq!(camera["state"], "error", "{camera}");
    assert_eq!(camera["error"]["code"], "worker_crashed", "{camera}");
    assert!(
        camera["error"]["message"]
            .as_str()
            .unwrap()
            .contains("exited"),
        "{camera}"
    );
}

/// A session killed without cleaning up leaves its descriptor, and its pid
/// may name another process by then: the name is free all the same.
#[cfg(unix)]
#[test]
fn a_killed_sessions_descriptor_does_not_hold_its_name() {
    let home = Home::new("stale");
    let started = home.ok(&["session", "start", "--name", "stale", "-c", "sim:0"]);
    let pid = started["pid"].as_i64().unwrap() as i32;
    // SAFETY: kill only sends a signal to the session this test started.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
    assert!(!support::cli::process_lives(pid));
    // Process 1 always runs.
    let descriptor = home.sessions.join("stale.json");
    let mut stale: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&descriptor).unwrap()).unwrap();
    stale["pid"] = json!(1);
    let leave = || {
        use std::{io::Write, os::unix::fs::OpenOptionsExt};
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&descriptor)
            .unwrap()
            .write_all(stale.to_string().as_bytes())
            .unwrap();
    };
    leave();
    assert_eq!(home.ok(&["session", "list"])["sessions"], json!([]));
    // Routing passes it by, and --keep starts a session of its own.
    assert_eq!(
        home.ok(&["get", "-c", "sim:0", "Width"])["values"]["Width"],
        640
    );
    home.ok(&["get", "-c", "sim:0", "Width", "--keep", "1m"]);
    assert_eq!(
        home.ok(&["session", "list"])["sessions"][0]["session"],
        "auto"
    );
    let gone = home.fails(3, &["session", "status", "stale"]);
    assert_eq!(gone["code"], "session_not_found");
    assert!(!descriptor.exists(), "what it left is removed");
    leave();
    let again = home.ok(&["session", "start", "--name", "stale", "-c", "sim:0"]);
    assert_eq!(again["started"], true);
    assert_ne!(again["pid"], json!(1));
    home.ok(&["session", "stop", "stale"]);
}

/// A session holds the cameras it was started with as long as it runs; a
/// camera a command connected with `--keep` goes once that keep runs out
/// unused, while the session goes on.
#[test]
fn a_session_keeps_its_own_cameras_and_lets_kept_ones_go() {
    let home = Home::new("keep-camera");
    home.ok(&["session", "start", "--name", "lab", "-c", "sim:0"]);
    let held = || {
        home.ok(&["session", "status", "lab"])["cameras"]
            .as_array()
            .unwrap()
            .iter()
            .map(|camera| camera["id"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    // A command's keep does not shorten what the session was started with.
    home.ok(&["get", "-c", "sim:0", "--keep", "1s", "Width"]);
    home.ok(&[
        "--session",
        "lab",
        "get",
        "-c",
        "sim:1",
        "--keep",
        "1s",
        "Width",
    ]);
    assert_eq!(held(), ["sim:0", "sim:1"]);
    let deadline = Instant::now() + Duration::from_secs(10);
    while held() != ["sim:0"] {
        assert!(Instant::now() < deadline, "{:?}", held());
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(held(), ["sim:0"]);
    home.ok(&["session", "stop", "lab"]);
}

/// A program that takes the port of a session that was killed learns
/// nothing from a command that names the session: no token, no camera
/// address with its sign-in, and no answer of its is taken for the
/// session's.
#[cfg(unix)]
#[test]
fn a_program_on_a_killed_sessions_port_is_sent_nothing() {
    use std::io::Read;
    let home = Home::new("squat");
    let started = home.ok(&["session", "start", "--name", "lab", "-c", "sim:0"]);
    let pid = started["pid"].as_i64().unwrap() as i32;
    let descriptor: serde_json::Value =
        serde_json::from_slice(&std::fs::read(home.sessions.join("lab.json")).unwrap()).unwrap();
    // SAFETY: kill only sends a signal to the session this test started.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
    let deadline = Instant::now() + Duration::from_secs(5);
    while support::cli::process_lives(pid) {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    let listener = std::net::TcpListener::bind(descriptor["address"].as_str().unwrap()).unwrap();
    listener.set_nonblocking(true).unwrap();
    let error = home.fails(
        3,
        &[
            "--session",
            "lab",
            "info",
            "-c",
            "rtsp://admin:Sup3rSecret@10.1.2.3/live",
        ],
    );
    assert_eq!(error["code"], "session_not_found", "{error}");
    let mut sent = Vec::new();
    if let Ok((mut stream, _)) = listener.accept() {
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        let _ = stream.read_to_end(&mut sent);
    }
    assert!(sent.is_empty(), "{}", String::from_utf8_lossy(&sent));
    assert!(!home.sessions.join("lab.json").exists());
}

/// Commands that start the same session at once share it: the cameras of
/// one capture, or several agents' calls.
#[test]
fn commands_that_start_a_session_at_once_share_it() {
    for round in 0..3 {
        let home = Home::new(&format!("keep-race-{round}"));
        let both = home.ok(&[
            "capture",
            "-c",
            "sim:0",
            "-c",
            "sim:1",
            "-o",
            "x/{camera}.png",
            "--keep",
            "1m",
        ]);
        assert_eq!(both["cameras"].as_array().unwrap().len(), 2, "{both}");
        let sessions = home.ok(&["session", "list"])["sessions"].clone();
        assert_eq!(sessions.as_array().unwrap().len(), 1, "{sessions}");
        assert_eq!(sessions[0]["cameras"].as_array().unwrap().len(), 2);
    }
}

/// A command without -c in a session uses its only camera; with none or
/// several it says which, never with credentials.
#[test]
fn a_session_names_its_only_camera_or_its_candidates() {
    let home = Home::new("only-camera");
    let _empty = home.serve("empty", &[]);
    let none = home.fails(3, &["--session", "empty", "get", "Width"]);
    assert_eq!(none["code"], "camera_not_found");
    assert_eq!(none["details"]["candidates"], json!([]));
    home.ok(&["--session", "empty", "get", "-c", "sim:one", "Width"]);
    assert_eq!(
        home.ok(&["--session", "empty", "get", "Width"])["camera"],
        "sim:one"
    );
    home.ok(&["--session", "empty", "get", "-c", "sim:two", "Width"]);
    let several = home.fails(2, &["--session", "empty", "get", "Width"]);
    assert_eq!(several["code"], "camera_ambiguous");
    assert_eq!(
        several["details"]["candidates"],
        json!([{"id": "sim:one"}, {"id": "sim:two"}])
    );
}
