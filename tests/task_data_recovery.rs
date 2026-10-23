//! Real process loss, WAL preservation and the public local recovery CLI.
#![cfg(all(feature = "parquet", unix))]
use capturefab::{
    dataset::task::TaskDataset,
    storage::{self, StoragePolicy},
};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

struct Directory(PathBuf);
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn command(root: &Path, executable: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut command = Command::new(executable);
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", root)
        .env("CAPTUREFAB_SESSION_DIR", root.join("state"))
        .env("CAPTUREFAB_CACHE_DIR", root.join("cache"));
    command
}
fn cli(root: &Path, args: &[&str]) -> Output {
    command(root, env!("CARGO_BIN_EXE_capturefab"))
        .arg("--json")
        .args(args)
        .output()
        .unwrap()
}
fn ok(root: &Path, args: &[&str]) -> Value {
    let output = cli(root, args);
    assert!(
        output.status.success(),
        "{args:?}: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice::<Value>(&output.stdout).unwrap()["result"].clone()
}

#[test]
fn retained_dataset_recovery_survives_process_loss_and_preserves_wal_in_copies() {
    if let Some(mode) = std::env::var_os("CAPTUREFAB_RECOVERY_CHILD") {
        let root = PathBuf::from(std::env::var_os("HOME").unwrap());
        std::fs::create_dir_all(root.join("captures")).unwrap();
        let mut data = TaskDataset::create_retained(
            &root.join("captures"),
            &StoragePolicy::default(),
            16 << 20,
        )
        .unwrap();
        data.initialize_catalog("Restart recovery").unwrap();
        data.record_event(
            "note",
            Some("sim:0"),
            None,
            "survives restart",
            &capturefab::context::Context::new(),
        )
        .unwrap();
        if mode == "closed" {
            data.close().unwrap();
        }
        let inventory = storage::quota_status().unwrap();
        let row = inventory["retained_namespaces"][0].clone();
        std::fs::write(
            root.join("workspace.json"),
            serde_json::to_vec(&row).unwrap(),
        )
        .unwrap();
        if mode == "live" {
            std::fs::write(root.join("owner.ready"), b"ready").unwrap();
            let deadline = Instant::now() + Duration::from_secs(30);
            while !root.join("owner.release").exists() {
                assert!(Instant::now() < deadline, "parent never released the owner");
                std::thread::sleep(Duration::from_millis(20));
            }
            data.close().unwrap();
        }
        // Deliberately bypass all Rust/native destructors for the interrupted
        // case. The parent must recover the original WAL, not a clean fixture.
        if mode == "crashed" {
            std::process::exit(0);
        }
        return;
    }
    for mode in ["closed", "crashed"] {
        let root = Directory(std::env::temp_dir().join(format!(
                "capturefab-recovery-{}-{}-{mode}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            )));
        std::fs::create_dir_all(&root.0).unwrap();
        let output = command(&root.0, std::env::current_exe().unwrap())
            .env("CAPTUREFAB_RECOVERY_CHILD", mode)
            .args([
                "--exact",
                "retained_dataset_recovery_survives_process_loss_and_preserves_wal_in_copies",
                "--nocapture",
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let fixture: Value =
            serde_json::from_slice(&std::fs::read(root.0.join("workspace.json")).unwrap()).unwrap();
        let id = fixture["id"].as_str().unwrap();
        let path = PathBuf::from(fixture["path"].as_str().unwrap());
        let listed = ok(&root.0, &["storage", "task-data", "list"]);
        assert_eq!(listed["workspaces"][0]["id"], id);
        assert_eq!(listed["workspaces"][0]["removable"], true, "{listed}");
        assert_eq!(
            ok(&root.0, &["storage", "status"])["allocated_bytes"],
            16 << 20
        );
        assert!(
            !cli(&root.0, &["storage", "task-data", "remove", id])
                .status
                .success()
        );
        assert!(path.is_dir(), "unconfirmed removal has no effect");
        let db = std::fs::read(path.join("task.duckdb")).unwrap();
        let wal = std::fs::read(path.join("task.duckdb.wal")).ok();
        if mode == "crashed" {
            assert!(
                wal.as_ref().is_some_and(|bytes| !bytes.is_empty()),
                "fixture must leave an actual WAL"
            );
        }
        let copy = root.0.join("saved-copy");
        let result = ok(
            &root.0,
            &["storage", "task-data", "copy", id, copy.to_str().unwrap()],
        );
        assert_eq!(result["copied"], id);
        assert_eq!(std::fs::read(copy.join("task.duckdb")).unwrap(), db);
        assert_eq!(std::fs::read(copy.join("task.duckdb.wal")).ok(), wal);
        assert_eq!(
            std::fs::read(path.join("task.duckdb")).unwrap(),
            db,
            "copy never mutates original"
        );
        assert_eq!(std::fs::read(path.join("task.duckdb.wal")).ok(), wal);
        assert!(
            !cli(
                &root.0,
                &["storage", "task-data", "copy", id, copy.to_str().unwrap()]
            )
            .status
            .success()
        );
        let conn = duckdb::Connection::open(copy.join("task.duckdb")).unwrap();
        let count: u64 = conn
            .query_row(
                "SELECT count(*) FROM events WHERE text='survives restart'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "{mode}: copied WAL recovers committed notes");
        drop(conn);
        let before = ok(&root.0, &["storage", "status"])["allocated_bytes"]
            .as_u64()
            .unwrap();
        let removed = ok(
            &root.0,
            &["storage", "task-data", "remove", id, "--confirmed-by-user"],
        );
        assert_eq!(removed, json!({"removed":id,"released_bytes":16 << 20}));
        assert!(!path.exists());
        assert_eq!(
            ok(&root.0, &["storage", "status"])["allocated_bytes"],
            before - (16 << 20)
        );
        assert!(
            ok(&root.0, &["storage", "task-data", "list"])["workspaces"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert!(copy.join("task.duckdb").is_file());
    }
}

/// Always release or reap the owner, including after a failed assertion.
struct LiveOwner {
    child: Child,
    release: PathBuf,
}
impl Drop for LiveOwner {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.release, b"release");
        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline {
            if self.child.try_wait().ok().flatten().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn retained_dataset_recovery_cli_refuses_live_owner_without_mutating_its_files() {
    let root = Directory(std::env::temp_dir().join(format!(
        "capturefab-recovery-live-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )));
    std::fs::create_dir_all(&root.0).unwrap();
    let log_path = root.0.join("owner.log");
    let log = std::fs::File::create(&log_path).unwrap();
    let mut owner = LiveOwner {
        child: command(&root.0, std::env::current_exe().unwrap())
            .env("CAPTUREFAB_RECOVERY_CHILD", "live")
            .args([
                "--exact",
                "retained_dataset_recovery_survives_process_loss_and_preserves_wal_in_copies",
                "--nocapture",
            ])
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log))
            .spawn()
            .unwrap(),
        release: root.0.join("owner.release"),
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    while !root.0.join("owner.ready").exists() {
        assert!(
            owner.child.try_wait().unwrap().is_none() && Instant::now() < deadline,
            "owner did not become ready: {}",
            std::fs::read_to_string(&log_path).unwrap()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let fixture: Value =
        serde_json::from_slice(&std::fs::read(root.0.join("workspace.json")).unwrap()).unwrap();
    let id = fixture["id"].as_str().unwrap();
    let path = PathBuf::from(fixture["path"].as_str().unwrap());
    let database = std::fs::read(path.join("task.duckdb")).unwrap();
    let wal = std::fs::read(path.join("task.duckdb.wal")).unwrap();
    assert!(
        !wal.is_empty(),
        "the live fixture must have actual committed WAL"
    );

    let listed = ok(&root.0, &["storage", "task-data", "list"]);
    let row = &listed["workspaces"][0];
    assert_eq!(row["id"], id);
    assert_eq!(row["removable"], false, "{listed}");
    assert!(
        row["error"].as_str().unwrap().contains("running session"),
        "{listed}"
    );
    let copy = root.0.join("saved-copy");
    for args in [
        vec!["storage", "task-data", "copy", id, copy.to_str().unwrap()],
        vec!["storage", "task-data", "remove", id, "--confirmed-by-user"],
    ] {
        let output = cli(&root.0, &args);
        assert!(
            !output.status.success(),
            "live recovery must refuse {args:?}"
        );
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(result["ok"], false, "{result}");
        assert!(
            result["error"]["message"]
                .as_str()
                .unwrap()
                .contains("running session"),
            "{result}"
        );
        assert!(
            !copy.exists(),
            "refused copy must not create an output folder"
        );
        assert_eq!(std::fs::read(path.join("task.duckdb")).unwrap(), database);
        assert_eq!(std::fs::read(path.join("task.duckdb.wal")).unwrap(), wal);
        assert_eq!(
            ok(&root.0, &["storage", "status"])["allocated_bytes"],
            16 << 20,
            "live-owner refusals must retain the full reservation"
        );
    }

    std::fs::write(&owner.release, b"release").unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = owner.child.try_wait().unwrap() {
            assert!(
                status.success(),
                "owner failed to close: {}",
                std::fs::read_to_string(&log_path).unwrap()
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "owner did not exit after release"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let listed = ok(&root.0, &["storage", "task-data", "list"]);
    assert_eq!(listed["workspaces"][0]["removable"], true, "{listed}");
    let saved = ok(
        &root.0,
        &["storage", "task-data", "copy", id, copy.to_str().unwrap()],
    );
    assert_eq!(saved["copied"], id);
    let conn = duckdb::Connection::open(copy.join("task.duckdb")).unwrap();
    let count: u64 = conn
        .query_row(
            "SELECT count(*) FROM events WHERE text='survives restart'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
    drop(conn);
    assert_eq!(
        ok(
            &root.0,
            &["storage", "task-data", "remove", id, "--confirmed-by-user"]
        ),
        json!({"removed":id,"released_bytes":16 << 20})
    );
    assert!(!path.exists());
    assert!(copy.join("task.duckdb").is_file());
}
