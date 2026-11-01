//! The warm default: a camera command leaves its camera open in the `auto`
//! session for a minute, so the next one finds it connected. The session
//! holds the connection, not the stream, ends unused, starts once when
//! several commands arrive together, names itself when it holds a camera
//! another command wants, and is skipped where no session can start.
mod support;

use serde_json::{Value, json};
use std::time::{Duration, Instant};
use support::cli::Home;

/// The running sessions, by name and pid.
fn sessions(home: &Home) -> Vec<(String, i64)> {
    home.ok(&["session", "list"])["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|session| {
            (
                session["session"].as_str().unwrap().to_owned(),
                session["pid"].as_i64().unwrap(),
            )
        })
        .collect()
}

#[test]
fn a_camera_command_keeps_its_camera_warm_for_the_next() {
    let home = Home::new("warm");
    let first = home.warm(&["--json", "get", "-c", "sim:0", "Width", "ExposureTime"]);
    let first = first.success();
    let started = sessions(&home);
    assert_eq!(started.len(), 1);
    assert_eq!(started[0].0, "auto");
    // The next command uses it, and finds what it holds, with the same
    // result a command of its own gets.
    let second = home.warm(&["--json", "get", "-c", "sim:0", "Width", "ExposureTime"]);
    assert_eq!(second.success(), first);
    assert_eq!(sessions(&home), started);
    let alone = home.ok(&["get", "-c", "sim:0", "Width", "ExposureTime"]);
    assert_eq!(alone, first);
    // It holds the connection, not the stream: after a capture the camera
    // stops.
    let shot = home.warm(&["--json", "capture", "-c", "sim:0", "-o", "shot.png"]);
    shot.success();
    let status = home.ok(&["session", "status", "auto"]);
    assert_eq!(status["cameras"][0]["state"], "idle", "{status}");
    // --keep 0 and --no-session open the camera for the command alone; the
    // session goes on as it was.
    for args in [
        &["--json", "get", "--keep", "0", "-c", "sim:1", "Width"][..],
        &["--json", "--no-session", "get", "-c", "sim:1", "Width"],
        &[
            "--json",
            "--no-session",
            "capture",
            "-c",
            "sim:1",
            "-o",
            "alone.png",
        ],
    ] {
        home.warm(args).success();
    }
    let held = home.ok(&["session", "status", "auto"])["cameras"]
        .as_array()
        .unwrap()
        .iter()
        .map(|camera| camera["id"].clone())
        .collect::<Vec<_>>();
    assert_eq!(held, [json!("sim:0")]);
    // `list` says which session holds a camera, and for how long unused.
    let listed = home.ok(&["list", "--kind", "sim"]);
    let held_by = &listed["cameras"][0]["held_by"];
    assert_eq!(held_by["session"], "auto", "{listed}");
    let left = held_by["idle_releases_in_s"].as_u64().unwrap();
    assert!((1..=60).contains(&left), "{listed}");
}

#[test]
fn a_short_keep_ends_the_session_unused() {
    let home = Home::new("warm-idle");
    home.warm(&["--json", "get", "--keep", "1s", "-c", "sim:0", "Width"])
        .success();
    let started = sessions(&home);
    assert_eq!(started.len(), 1);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !sessions(&home).is_empty() {
        assert!(Instant::now() < deadline, "the auto session stayed");
        std::thread::sleep(Duration::from_millis(100));
    }
    #[cfg(unix)]
    assert!(!support::cli::process_lives(started[0].1 as i32));
    // The next command starts it again.
    home.warm(&["--json", "get", "-c", "sim:0", "Width"])
        .success();
    assert_eq!(sessions(&home).len(), 1);
}

#[test]
fn commands_arriving_together_start_one_session() {
    let home = Home::new("warm-together");
    let children = (0..4)
        .map(|index| {
            home.warm_command()
                .args([
                    "--json",
                    "get",
                    "-c",
                    &format!("sim:{}", index % 2),
                    "Width",
                ])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect::<Vec<_>>();
    for child in children {
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["values"]["Width"], 640, "{value}");
    }
    let started = sessions(&home);
    assert_eq!(started.len(), 1, "{started:?}");
    let held = home.ok(&["session", "status", "auto"])["cameras"]
        .as_array()
        .unwrap()
        .len();
    assert_eq!(held, 2);
}

#[test]
fn a_camera_the_session_holds_names_it_to_another_command() {
    let home = Home::new("warm-busy");
    let hooks = [("CAPTUREFAB_TEST_HOOKS", "1")];
    // A simulator that, like a GigE camera, one process at a time owns.
    let camera = "sim:0?exclusive=1";
    let run = |args: &[&str]| {
        support::cli::Run::of_output(
            home.warm_command().envs(hooks).args(args).output().unwrap(),
            args,
        )
    };
    run(&["--json", "get", "-c", camera, "Width"]).success();
    let error = run(&["--json", "--no-session", "get", "-c", camera, "Width"]).failure(3);
    assert_eq!(error["code"], "camera_busy", "{error}");
    let held_by = &error["details"]["held_by"];
    assert_eq!(held_by["session"], "auto", "{error}");
    assert!(
        held_by["idle_releases_in_s"].as_u64().unwrap() <= 60,
        "{error}"
    );
    let hint = error["hint"].as_str().unwrap();
    assert!(
        hint.contains("capturefab session stop auto") && hint.contains("--keep 0"),
        "{hint}"
    );
    // Any other command uses the session that holds it.
    run(&["--json", "get", "--keep", "0", "-c", camera, "Width"]).success();
}

#[test]
fn a_change_after_a_capture_finds_a_lingering_camera_stopped() {
    let home = Home::new("warm-linger-set");
    let hooks = [("CAPTUREFAB_TEST_HOOKS", "1")];
    // A simulator that, like a webcam, goes on streaming a moment after a
    // capture for the next one.
    let camera = "sim:0?linger=1";
    let run = |args: &[&str]| {
        support::cli::Run::of_output(
            home.warm_command().envs(hooks).args(args).output().unwrap(),
            args,
        )
    };
    let state = || home.ok(&["session", "status", "auto"])["cameras"][0]["state"].clone();
    run(&["--json", "capture", "-c", camera, "-o", "a.png"]).success();
    assert_eq!(state(), "streaming");
    // The width is locked while the camera streams; a change stops a
    // stream that only lingers, as it finds the camera alone.
    let checked = run(&["--json", "set", "-c", camera, "--dry-run", "Width=320"]).success();
    assert_eq!(checked["plan"][0]["name"], "Width", "{checked}");
    run(&["--json", "capture", "-c", camera, "-o", "b.png"]).success();
    let changed = run(&["--json", "set", "-c", camera, "Width=320", "Height=240"]).success();
    assert_eq!(changed["changed"][0]["to"], 320, "{changed}");
    assert_eq!(state(), "idle");
    let shot = run(&["--json", "capture", "-c", camera, "-o", "c.png"]).success();
    assert_eq!(shot["width"], 320, "{shot}");
}

#[test]
fn a_command_without_a_camera_finds_it_as_it_would_alone() {
    let home = Home::new("warm-default-camera");
    // No camera this test can find: FFmpeg lists no host camera, and no
    // network is searched.
    let quiet = [("CAPTUREFAB_FFMPEG", "/usr/bin/false")];
    let run = |args: &[&str]| {
        support::cli::Run::of_output(
            home.warm_command().envs(quiet).args(args).output().unwrap(),
            args,
        )
    };
    run(&["--json", "get", "-c", "sim:0", "Width"]).success();
    assert_eq!(sessions(&home).len(), 1);
    // The auto session holds sim:0, which no command takes for its default
    // camera: without -c, the result is the one a command alone gets.
    let alone = run(&["--json", "--no-session", "get", "Width"]);
    let warm = run(&["--json", "get", "Width"]);
    assert_eq!(warm.code, alone.code, "{}", warm.stdout);
    assert_eq!(warm.stdout, alone.stdout);
}

#[test]
fn a_session_command_without_a_name_leaves_the_auto_session_aside() {
    let home = Home::new("warm-unnamed");
    home.ok(&["session", "start", "-c", "sim:0"]);
    home.warm(&["--json", "get", "-c", "sim:1", "Width"])
        .success();
    assert_eq!(sessions(&home).len(), 2);
    assert_eq!(home.ok(&["session", "status"])["session"], "default");
    assert_eq!(home.ok(&["session", "stop"])["session"], "default");
    // Alone, the auto session is the one.
    assert_eq!(home.ok(&["session", "status"])["session"], "auto");
}

#[test]
fn verbose_shows_what_the_session_logged_for_the_command() {
    let home = Home::new("warm-verbose");
    let first = home.warm(&["-v", "get", "-c", "sim:0", "Width"]);
    assert_eq!(first.code, Some(0), "{}", first.stderr);
    assert!(
        first.stderr.contains("[info] Connected to sim:0"),
        "{}",
        first.stderr
    );
    // The next command connects nothing, and says what it did.
    let next = home.warm(&["-v", "set", "-c", "sim:0", "Width=320"]);
    assert_eq!(next.code, Some(0), "{}", next.stderr);
    assert!(!next.stderr.contains("Connected"), "{}", next.stderr);
    assert!(
        next.stderr.contains("[info] Set Width = 320"),
        "{}",
        next.stderr
    );
}

#[cfg(unix)]
#[test]
fn without_a_writable_runtime_folder_a_command_opens_its_camera_alone() {
    use std::os::unix::fs::PermissionsExt;
    let home = Home::new("warm-read-only");
    let locked = home.root.join("read-only");
    std::fs::create_dir_all(&locked).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500)).unwrap();
    // Root can write anywhere; nothing to test then.
    if std::fs::create_dir(locked.join("probe")).is_ok() {
        return;
    }
    let sessions_dir = locked.join("sessions");
    let run = |args: &[&str]| {
        support::cli::Run::of_output(
            home.warm_command()
                .env("CAPTUREFAB_SESSION_DIR", &sessions_dir)
                .args(args)
                .output()
                .unwrap(),
            args,
        )
    };
    let quiet = run(&["--json", "get", "-c", "sim:0", "Width"]);
    assert_eq!(quiet.success()["values"]["Width"], 640);
    assert!(quiet.stderr.is_empty(), "{}", quiet.stderr);
    let told = run(&["-v", "get", "-c", "sim:0", "Width"]);
    assert_eq!(told.code, Some(0), "{}", told.stderr);
    assert!(
        told.stderr.contains("for this command alone"),
        "{}",
        told.stderr
    );
    assert!(!sessions_dir.exists());
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
#[ignore = "requires FFmpeg; set CAPTUREFAB_FFMPEG to the executable"]
fn a_live_stream_decodes_only_while_frames_are_needed_and_a_moment_after() {
    let home = Home::new("warm-linger");
    let port = std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let stream = format!("udp://127.0.0.1:{port}");
    // A live stream of its own, sent until the test ends.
    struct Ends(std::process::Child);
    impl Drop for Ends {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let _sender = Ends(
        std::process::Command::new(support::cli::ffmpeg())
            .args([
                "-v",
                "error",
                "-re",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=64x48:rate=10",
            ])
            .args([
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-tune",
                "zerolatency",
            ])
            .args(["-g", "5", "-f", "mpegts", &stream])
            .stdin(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    let state = || home.ok(&["session", "status", "auto"])["cameras"][0]["state"].clone();
    home.warm(&["--json", "inspect", "-c", &stream, "--preview", "none"])
        .success();
    // FFmpeg goes on decoding for the next command of a loop...
    assert_eq!(state(), "streaming");
    home.warm(&["--json", "inspect", "-c", &stream, "--preview", "none"])
        .success();
    // ...and stops a few seconds after the last.
    let deadline = Instant::now() + Duration::from_secs(20);
    while state() != "idle" {
        assert!(Instant::now() < deadline, "the stream kept decoding");
        std::thread::sleep(Duration::from_millis(250));
    }
    // A command that takes no frame opens the stream to connect, and lets
    // it go a few seconds later.
    #[cfg(unix)]
    {
        home.ok(&["session", "stop", "auto"]);
        let decoding = || {
            std::process::Command::new("pgrep")
                .args(["-f", &format!("[-]i {stream}")])
                .stdout(std::process::Stdio::null())
                .status()
                .unwrap()
                .success()
        };
        assert!(!decoding());
        home.warm(&["--json", "get", "-c", &stream, "Width"])
            .success();
        assert!(decoding());
        let deadline = Instant::now() + Duration::from_secs(20);
        while decoding() {
            assert!(Instant::now() < deadline, "FFmpeg kept decoding");
            std::thread::sleep(Duration::from_millis(250));
        }
        assert_eq!(state(), "idle");
    }
}
