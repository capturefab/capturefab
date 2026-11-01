//! Sessions: started in the background or served in the foreground, found
//! by the cameras they hold, kept by --keep, running --background jobs, and
//! ended by `session stop`, idle time or signals.
mod support;

use serde_json::json;
use std::time::{Duration, Instant};
use support::cli::Home;

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
    let list = home.ok(&["session", "list"]);
    assert_eq!(list["sessions"][0]["session"], "bench");
    let status = home.ok(&["session", "status", "bench"]);
    assert_eq!(status["pid"], pid);
    assert_eq!(status["cameras"].as_array().unwrap().len(), 2);
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
    // A failed job reports its own reason.
    home.ok(&[
        "set",
        "-c",
        "sim:0",
        "TriggerMode=On",
        "TriggerSource=Line1",
    ]);
    let triggered = home.ok(&[
        "capture",
        "-c",
        "sim:0",
        "--timeout",
        "200ms",
        "-o",
        "never.png",
        "--background",
    ]);
    let id = triggered["job"]["id"].as_str().unwrap().to_owned();
    let failed = home.fails(4, &["jobs", "wait", &id, "--timeout", "10s"]);
    assert_eq!(failed["code"], "trigger_timeout");
    assert_eq!(failed["details"]["job"]["error"]["code"], "trigger_timeout");
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
}
