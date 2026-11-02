//! Hosts that keep little writable: a read-only home and root file system
//! with only a temporary folder (or nothing) writable, as on Yocto-style and
//! immutable images, and a process told to use one thread. Each command degrades only what it needs: sessions move to the
//! temporary folder, or commands open their cameras alone; presets and
//! sign-ins say which variable gives them a folder; captures to a writable
//! folder always work.
#![cfg(unix)]
mod support;

use serde_json::Value;
use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
};
use support::cli::{Home, Run};

/// A host whose home is read-only and whose temporary folder is `temp`:
/// no CAPTUREFAB_* or XDG_* folder variables, the warm default on.
struct ReadOnly {
    home: Home,
    read_only: PathBuf,
    temp: PathBuf,
}
impl ReadOnly {
    /// None when this user writes anywhere, as root does: nothing to test.
    fn new(name: &str, temp_writable: bool) -> Option<Self> {
        let home = Home::new(name);
        let read_only = home.root.join("read-only");
        let temp = home.root.join("tmp");
        std::fs::create_dir_all(read_only.join("home")).unwrap();
        std::fs::create_dir_all(&temp).unwrap();
        for dir in [read_only.join("home"), read_only.clone()] {
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        }
        if !temp_writable {
            std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o500)).unwrap();
        }
        let host = Self {
            home,
            read_only,
            temp,
        };
        if std::fs::create_dir(host.read_only.join("probe")).is_ok() {
            return None;
        }
        Some(host)
    }
    fn command(&self) -> Command {
        let mut command = self.home.warm_command();
        for variable in [
            "CAPTUREFAB_SESSION_DIR",
            "CAPTUREFAB_STATE_DIR",
            "CAPTUREFAB_CACHE_DIR",
            "XDG_RUNTIME_DIR",
            "XDG_STATE_HOME",
            "XDG_CACHE_HOME",
        ] {
            command.env_remove(variable);
        }
        command
            .env("HOME", self.read_only.join("home"))
            .env("TMPDIR", &self.temp);
        // On Linux the login's /run/user is a candidate without
        // XDG_RUNTIME_DIR; one that cannot be written keeps the test out
        // of the user's own.
        if cfg!(target_os = "linux") {
            command.env("XDG_RUNTIME_DIR", self.read_only.join("run"));
        }
        command
    }
    fn run(&self, args: &[&str]) -> Run {
        self.run_with(&[], args)
    }
    fn run_with(&self, env: &[(&str, &Path)], args: &[&str]) -> Run {
        let mut command = self.command();
        for (name, value) in env {
            command.env(name, value);
        }
        Run::of_output(command.args(args).output().unwrap(), args)
    }
    fn ok(&self, args: &[&str]) -> Value {
        self.run(&[&["--json"][..], args].concat()).success()
    }
    fn fails(&self, exit: i32, args: &[&str]) -> Value {
        self.run(&[&["--json"][..], args].concat()).failure(exit)
    }
    /// This user's folder in the temporary folder.
    fn private_temp(&self) -> PathBuf {
        // SAFETY: geteuid has no preconditions.
        let uid = unsafe { libc::geteuid() };
        self.temp.join(format!("capturefab-{uid}"))
    }
}
impl Drop for ReadOnly {
    fn drop(&mut self) {
        let _ = self.run(&["--json", "session", "stop", "auto"]);
        for dir in [&self.read_only, &self.read_only.join("home"), &self.temp] {
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        }
    }
}

#[test]
fn a_read_only_home_keeps_sessions_in_the_temporary_folder() {
    let Some(host) = ReadOnly::new("env-read-only-home", true) else {
        return;
    };
    let first = host.ok(&["get", "-c", "sim:0", "Width"]);
    assert_eq!(first["values"]["Width"], 640);
    // The warm default works: a session in this user's temporary folder
    // holds the camera for the next command.
    let sessions = host.ok(&["session", "list"]);
    assert_eq!(sessions["sessions"][0]["session"], "auto", "{sessions}");
    let runtime = host.private_temp();
    assert!(runtime.join("auto.json").exists());
    let mode = std::fs::metadata(&runtime).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o700);
    let next = host.run(&["-v", "set", "-c", "sim:0", "Width=320"]);
    assert_eq!(next.code, Some(0), "{}", next.stderr);
    assert!(!next.stderr.contains("Connected"), "{}", next.stderr);
    // Captures to a writable folder work; previews go to the temporary
    // folder.
    let shot = host.ok(&["capture", "-c", "sim:0", "-o", "shots/"]);
    assert_eq!(shot["count"], 1);
    assert!(Path::new(shot["files"][0].as_str().unwrap()).exists());
    let inspected = host.ok(&["inspect", "-c", "sim:0"]);
    let preview = inspected["preview"].as_str().unwrap();
    assert!(
        Path::new(preview).starts_with(runtime.join("inspect")),
        "{preview}"
    );
    // State has nowhere to go: presets and saved sign-ins say which
    // variable gives them a folder, and work once it does.
    for args in [&["preset", "list"][..], &["login", "list"]] {
        let error = host.fails(3, args);
        assert_eq!(error["code"], "permission_denied", "{error}");
        assert!(
            error["hint"]
                .as_str()
                .unwrap()
                .contains("CAPTUREFAB_STATE_DIR="),
            "{error}"
        );
    }
    let state = host.home.root.join("data");
    let presets = host
        .run_with(
            &[("CAPTUREFAB_STATE_DIR", &state)],
            &["--json", "preset", "list"],
        )
        .success();
    assert_eq!(presets["presets"], serde_json::json!([]));
    // doctor names the fallbacks in use and why.
    let doctor = host.ok(&["doctor"]);
    assert_eq!(doctor["folders"]["runtime"]["from"], "temp", "{doctor}");
    assert_eq!(
        doctor["folders"]["runtime"]["path"],
        runtime.to_str().unwrap()
    );
    assert!(
        doctor["folders"]["runtime"]["passed"]
            .as_array()
            .unwrap()
            .iter()
            .all(|passed| passed["why"].as_str().is_some()),
        "{doctor}"
    );
    assert_eq!(doctor["folders"]["cache"]["from"], "runtime", "{doctor}");
    assert!(doctor["folders"]["state"]["error"].is_string(), "{doctor}");
    assert_eq!(doctor["warm_sessions"], true);
    let check = |name: &str| {
        doctor["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|check| check["name"] == name)
            .cloned()
            .unwrap_or_else(|| panic!("{doctor}"))
    };
    assert_eq!(check("sessions")["status"], "ok");
    assert_eq!(check("session_storage")["status"], "warn");
}

#[test]
fn with_nothing_writable_each_command_opens_its_camera_alone() {
    let Some(host) = ReadOnly::new("env-nothing-writable", false) else {
        return;
    };
    let got = host.run(&["--json", "get", "-c", "sim:0", "Width"]);
    assert_eq!(got.success()["values"]["Width"], 640);
    assert!(got.stderr.is_empty(), "{}", got.stderr);
    let told = host.run(&["-v", "get", "-c", "sim:0", "Width"]);
    assert_eq!(told.code, Some(0), "{}", told.stderr);
    assert!(
        told.stderr.contains("for this command alone"),
        "{}",
        told.stderr
    );
    // A capture to a writable data folder always works.
    let data = host.home.root.join("data");
    std::fs::create_dir_all(&data).unwrap();
    let out = data.join("shot.png");
    let shot = host.ok(&["capture", "-c", "sim:0", "-o", out.to_str().unwrap()]);
    assert_eq!(shot["count"], 1);
    assert!(out.exists());
    // A preview needs a folder: the error says how to do without.
    let error = host.fails(3, &["inspect", "-c", "sim:0"]);
    assert_eq!(error["code"], "permission_denied", "{error}");
    assert!(
        error["hint"].as_str().unwrap().contains("--preview none"),
        "{error}"
    );
    let measured = host.ok(&["inspect", "-c", "sim:0", "--preview", "none"]);
    assert!(measured["exposure"].is_object(), "{measured}");
    let preview = data.join("preview.jpg");
    let written = host.ok(&[
        "inspect",
        "-c",
        "sim:0",
        "--preview",
        preview.to_str().unwrap(),
    ]);
    assert_eq!(written["preview"], preview.to_str().unwrap());
    // --keep needs a session, which cannot start: the error names the
    // variable that gives it a folder.
    let error = host.fails(3, &["get", "-c", "sim:0", "Width", "--keep", "1m"]);
    assert_eq!(error["code"], "permission_denied", "{error}");
    assert!(
        error["hint"]
            .as_str()
            .unwrap()
            .contains("CAPTUREFAB_SESSION_DIR"),
        "{error}"
    );
    let doctor = host.ok(&["doctor"]);
    assert_eq!(doctor["warm_sessions"], false, "{doctor}");
    assert!(
        doctor["folders"]["runtime"]["error"].is_string(),
        "{doctor}"
    );
    // Nothing was written anywhere.
    assert!(std::fs::read_dir(&host.temp).unwrap().next().is_none());
}

#[test]
fn one_encoder_thread_still_captures_every_frame_of_a_burst() {
    let home = Home::new("env-one-thread");
    // The burst queue holds what the memory left allows, which on any host
    // fits ten 640x480 frames while one thread encodes them.
    let burst = home
        .run_env(
            &[("CAPTUREFAB_THREADS", "1")],
            &[
                "--json", "capture", "-c", "sim:0", "-n", "10", "-o", "burst/",
            ],
        )
        .success();
    assert_eq!(burst["count"], 10, "{burst}");
    assert_eq!(burst["dropped"], 0, "{burst}");
    let doctor = home.run_env(&[("CAPTUREFAB_THREADS", "1")], &["--json", "doctor"]);
    let doctor = doctor.success();
    assert_eq!(doctor["host"]["threads"], 1, "{doctor}");
    assert!(doctor["host"]["cpus"].as_u64().unwrap() >= 1);
}

#[test]
#[ignore = "requires FFmpeg; set CAPTUREFAB_FFMPEG to the executable"]
fn a_read_only_home_records_to_a_data_folder() {
    let Some(host) = ReadOnly::new("env-record", true) else {
        return;
    };
    let data = host.home.root.join("data");
    std::fs::create_dir_all(&data).unwrap();
    let clip = data.join("clip.mkv");
    let result = host.ok(&[
        "record",
        "-c",
        "sim:0",
        "-o",
        clip.to_str().unwrap(),
        "--for",
        "1s",
        "--fps",
        "10",
    ]);
    assert_eq!(result["finalized"], true, "{result}");
    assert!(result["frames"].as_u64().unwrap() > 0, "{result}");
    assert!(support::cli::decodes(&clip));
}

#[test]
fn a_preset_applies_through_a_session_that_keeps_state_elsewhere() {
    // A service with a state folder of its own, reached from a shell that
    // sets only CAPTUREFAB_SESSION_DIR: the preset this shell saved is the
    // one applied, not one of the same name in the service's folder.
    let home = Home::new("env-state-elsewhere");
    let service_state = home.root.join("service-state");
    let mut service = home.command();
    service.env("CAPTUREFAB_STATE_DIR", &service_state);
    let _session = home.serve_in(&mut service, "line", &["sim:0"], &[]);
    let at = |width: &str| {
        home.ok(&[
            "set",
            "--session",
            "line",
            "-c",
            "sim:0",
            &format!("Width={width}"),
        ]);
    };
    at("320");
    home.ok(&[
        "preset",
        "save",
        "narrow",
        "--session",
        "line",
        "-c",
        "sim:0",
    ]);
    assert!(
        home.sessions
            .join("presets")
            .read_dir()
            .unwrap()
            .next()
            .is_some()
    );
    at("640");
    let applied = home.ok(&[
        "preset",
        "apply",
        "narrow",
        "--session",
        "line",
        "-c",
        "sim:0",
    ]);
    assert_eq!(applied["applied"], 1, "{applied}");
    let width = home.ok(&["get", "--session", "line", "-c", "sim:0", "Width"]);
    assert_eq!(width["values"]["Width"], 320, "{width}");
}
