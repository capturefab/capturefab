use super::*;
use crate::dataset::{OpenMode, Slot};
use std::{
    path::PathBuf,
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

struct Fixture {
    root: PathBuf,
    slot: Arc<Slot>,
    source: Option<Reader>,
}
fn actual_home() -> String {
    std::env::var(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).unwrap()
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "capturefab-model-snapshot-{}-{}",
            std::process::id(),
            crate::dataset::db::random_name().unwrap()
        ));
        crate::ipc::ensure_private_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let slot = Arc::new(Slot::new());
        let status = slot
            .open(&root.join("source.duckdb"), OpenMode::Create, None)
            .unwrap();
        let source = slot.reader_if(status.slot_id.as_deref().unwrap()).unwrap();
        source.conn.execute_batch("INSERT INTO captures(wall_ms,camera_id,serial,source,format,bytes,path,file_name,origin) VALUES(1500,'sim:0','SERIAL_MODEL_CANARY','https://user:LOCATOR_MODEL_PASSWORD@example.test/private-camera-path?token=LOCATOR_MODEL_TOKEN','png',1,'/Users/HOME_MODEL_CANARY/private-capture.png','capture.png','capture')").unwrap();
        let home = actual_home();
        source
            .conn
            .execute(
                "UPDATE captures SET path=?",
                [format!("{home}/consented-capture.png")],
            )
            .unwrap();
        Self {
            root,
            slot,
            source: Some(source),
        }
    }
    fn source(&self) -> &Reader {
        self.source.as_ref().unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.source.take();
        let _ = self.slot.close();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
fn limits() -> ModelQueryLimits {
    ModelQueryLimits {
        snapshot: snapshot::SnapshotLimits {
            max_rows: 10_000,
            max_arrow_bytes: 8 * 1024 * 1024,
            max_batch_bytes: 8 * 1024 * 1024,
            timeout_ms: 3000,
        },
        max_result_rows: 100,
        max_result_bytes: 32 * 1024,
    }
}
fn run(fixture: &Fixture, sql: &str) -> Result<Value> {
    // Semantic checks use the production budget on slower debug runners.
    // Deadline/cancellation tests below keep their explicit short bounds.
    let mut bounds = limits();
    bounds.snapshot.timeout_ms = super::super::query::DEFAULT_TIMEOUT_MS;
    query(fixture.source(), sql, bounds, &AtomicBool::new(false))
}

#[test]
fn projected_snapshot_preserves_captures_but_not_stored_private_metadata() {
    let fixture = Fixture::new();
    let protected = run(
        &fixture,
        "SELECT serial,replace(serial,'SERIAL_',''),reverse(source),hex(path) FROM captures",
    )
    .unwrap();
    assert!(protected["rows"][0][0].is_null());
    let text = protected.to_string();
    for secret in [
        "SERIAL_MODEL_CANARY",
        "LOCATOR_MODEL_PASSWORD",
        "LOCATOR_MODEL_TOKEN",
        "LOCATOR_MODEL_CLIENT",
        "private-camera-path",
        "private-topic",
    ] {
        assert!(!text.contains(secret), "stored metadata leaked: {secret}");
        assert!(
            !text.contains(&secret.chars().rev().collect::<String>()),
            "reversed stored metadata leaked: {secret}"
        );
        let hex = secret
            .as_bytes()
            .iter()
            .map(|byte| format!("{byte:02X}"))
            .collect::<String>();
        assert!(!text.contains(&hex), "hex stored metadata leaked: {secret}");
    }
    let home = actual_home();
    assert!(!text.contains(&home), "actual home prefix leaked");
    let home_hex = home
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<String>();
    assert!(!text.contains(&home_hex), "encoded home prefix leaked");
    // The source remains lossless; this is projection, not source mutation.
    let serial: String = fixture
        .source()
        .conn
        .query_row("SELECT serial FROM captures", [], |row| row.get(0))
        .unwrap();
    assert_eq!(serial, "SERIAL_MODEL_CANARY");
    assert_eq!(
        run(&fixture, "SELECT 'caller supplied literal'").unwrap()["rows"][0][0],
        "caller supplied literal"
    );
}

#[test]
fn reader_wrapper_uses_private_projection_and_default_overall_deadline() {
    let fixture = Fixture::new();
    let reply = fixture
        .source()
        .query_model_bounded("SELECT camera_id,serial FROM captures", 10, 0, 2048)
        .unwrap();
    assert_eq!(reply["rows"][0], serde_json::json!(["sim:0", null]));
    assert!(
        fixture
            .source()
            .query_model_bounded(
                "SELECT 1",
                10,
                super::super::query::MAX_TIMEOUT_MS + 1,
                2048
            )
            .is_err()
    );
    assert!(!reply.to_string().contains("SERIAL_MODEL_CANARY"));
}

#[test]
fn stored_file_urls_project_home_before_sql_decoding_or_transforms() {
    let fixture = Fixture::new();
    let home = actual_home().replace('\\', "/");
    let url_home = home.trim_start_matches('/');
    let escaped_home = url_home
        .as_bytes()
        .iter()
        .map(|byte| {
            if *byte == b'/' {
                "/".into()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect::<String>();
    let expected = "file:~/consented-capture.png";
    let hex = expected
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<String>();
    let reversed = expected.chars().rev().collect::<String>();
    for url in [
        format!("file:///{url_home}/consented-capture.png"),
        format!("file://localhost/{url_home}/consented-capture.png"),
        format!("file:///{escaped_home}/consented-capture.png"),
    ] {
        fixture
            .source()
            .conn
            .execute("UPDATE captures SET path=?", [&url])
            .unwrap();
        let reply = run(&fixture, "SELECT path,reverse(path),hex(path),url_decode(path),hex(url_decode(path)),reverse(url_decode(path)) FROM captures").unwrap();
        assert_eq!(
            reply["rows"][0],
            serde_json::json!([expected, reversed, hex, expected, hex, reversed]),
            "model SQL must see only the already-projected file location"
        );
        let original: String = fixture
            .source()
            .conn
            .query_row("SELECT path FROM captures", [], |row| row.get(0))
            .unwrap();
        assert_eq!(original, url, "the source catalog must remain lossless");
    }
}

#[test]
fn generated_source_columns_are_rejected_before_preserved_values_are_scanned() {
    let fixture = Fixture::new();
    fixture.source().conn.execute_batch("DROP TABLE events; CREATE TABLE events(id UBIGINT,wall_ms UBIGINT,kind VARCHAR,camera_id VARCHAR,source_id VARCHAR,text VARCHAR GENERATED ALWAYS AS (current_setting('secret_directory')),context VARCHAR); INSERT INTO events(id,wall_ms,kind) VALUES(1,1400,'note')").unwrap();
    let source_secret: String = fixture
        .source()
        .conn
        .query_row("SELECT text FROM events", [], |row| row.get(0))
        .unwrap();
    assert!(!source_secret.is_empty());
    let error = run(&fixture, "SELECT text FROM events").unwrap_err();
    assert!(
        error.to_string().contains("schema") || error.to_string().contains("column"),
        "{error:#}"
    );
    assert!(!error.to_string().contains(&source_secret), "{error:#}");
}

#[test]
fn source_schema_defaults_types_order_and_extra_columns_fail_before_scanning() {
    for alteration in [
        "ALTER TABLE events ALTER text SET DEFAULT 'PRIVATE_DEFAULT_CANARY'",
        "ALTER TABLE events ALTER wall_ms TYPE DECIMAL(24,2)",
        "ALTER TABLE events ADD extra VARCHAR",
        "DROP TABLE events; CREATE TABLE events(wall_ms UBIGINT,id UBIGINT,kind VARCHAR,camera_id VARCHAR,source_id VARCHAR,text VARCHAR,context VARCHAR)",
    ] {
        let fixture = Fixture::new();
        fixture.source().conn.execute_batch(alteration).unwrap();
        let error = run(&fixture, "SELECT text FROM events").unwrap_err();
        assert!(error.to_string().contains("schema"), "{error:#}");
        assert!(!error.to_string().contains("PRIVATE_DEFAULT_CANARY"));
    }
}

#[test]
fn source_macros_cannot_shadow_fixed_catalog_introspection() {
    let fixture = Fixture::new();
    fixture.source().conn.execute_batch("CREATE MACRO main.current_database() AS 'PRIVATE_MACRO_CANARY'; CREATE MACRO main.duckdb_tables() AS TABLE SELECT 'PRIVATE_MACRO_CANARY' AS sql; CREATE MACRO main.duckdb_columns() AS TABLE SELECT 'PRIVATE_MACRO_CANARY' AS column_name").unwrap();
    let reply = run(&fixture, "SELECT camera_id FROM captures").unwrap();
    assert_eq!(reply["rows"][0][0], "sim:0");
    assert!(!reply.to_string().contains("PRIVATE_MACRO_CANARY"));
}

#[test]
fn result_bounds_and_snapshot_limits_never_fall_back_to_shared_sql() {
    let fixture = Fixture::new();
    let small = ModelQueryLimits {
        max_result_rows: 2,
        max_result_bytes: 512,
        ..limits()
    };
    let reply = query(
        fixture.source(),
        "SELECT i,repeat('x',100) FROM range(100) t(i)",
        small,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!(serde_json::to_vec(&reply).unwrap().len() <= 512);
    assert_eq!(reply["truncated"], true);
    assert!(reply["rows"].as_array().unwrap().len() <= 2);
    for bounds in [
        snapshot::SnapshotLimits {
            max_rows: 1,
            ..limits().snapshot
        },
        snapshot::SnapshotLimits {
            max_arrow_bytes: 1,
            ..limits().snapshot
        },
        snapshot::SnapshotLimits {
            max_batch_bytes: 1,
            ..limits().snapshot
        },
    ] {
        let error = query(
            fixture.source(),
            "SELECT serial FROM captures",
            ModelQueryLimits {
                snapshot: bounds,
                ..limits()
            },
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert!(error.to_string().contains("limit"), "{error:#}");
        assert!(!error.to_string().contains("SERIAL_MODEL_CANARY"));
    }
}

#[test]
fn cancellation_and_one_absolute_deadline_cover_the_entire_private_query() {
    let fixture = Fixture::new();
    assert!(
        query(
            fixture.source(),
            "SELECT 1",
            limits(),
            &AtomicBool::new(true)
        )
        .unwrap_err()
        .to_string()
        .contains("cancelled")
    );
    assert!(remaining_query_ms(Instant::now()).is_err());
    let cancel = AtomicBool::new(false);
    let started = Instant::now();
    let result = std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(100));
            cancel.store(true, Ordering::Release);
        });
        query(
            fixture.source(),
            "SELECT sum(sin(i::DOUBLE)) FROM range(100000000000) t(i)",
            limits(),
            &cancel,
        )
    });
    assert!(result.is_err());
    assert!(started.elapsed() < Duration::from_secs(3));
    let timeout = ModelQueryLimits {
        snapshot: snapshot::SnapshotLimits {
            timeout_ms: 1,
            ..limits().snapshot
        },
        ..limits()
    };
    assert!(
        query(
            fixture.source(),
            "SELECT sum(sin(i::DOUBLE)) FROM range(100000000000) t(i)",
            timeout,
            &AtomicBool::new(false)
        )
        .is_err()
    );
    // The shared reader remains usable after target-only interruption.
    assert_eq!(
        fixture
            .source()
            .conn
            .query_row("SELECT count(*) FROM captures", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[test]
fn isolated_subprocess_sanitizes_inherited_metadata_and_cannot_write_or_spill() {
    let root = std::env::temp_dir().join(format!(
        "capturefab-model-environment-{}-{}",
        std::process::id(),
        crate::dataset::db::random_name().unwrap()
    ));
    crate::ipc::ensure_private_dir(&root).unwrap();
    let home = root.join("HOME_ENV_MODEL_CANARY");
    crate::ipc::ensure_private_dir(&home).unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "dataset::model_snapshot::tests::environment_probe_child",
            "--nocapture",
        ])
        .current_dir(&root)
        .env("CAPTUREFAB_MODEL_METADATA_PROBE", "1")
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("USER", "USER_ENV_MODEL_CANARY")
        .env("USERNAME", "USER_ENV_MODEL_CANARY")
        .env("HTTP_PROXY", "http://USER_ENV_MODEL_CANARY:PASS_ENV_MODEL_CANARY@proxy.invalid/PRIVATE_ENV_MODEL_CANARY")
        .env("http_proxy", "http://LOWER_ENV_MODEL_CANARY.invalid")
        .env("http_proxy_username", "USER_ENV_MODEL_CANARY")
        .env("http_proxy_password", "PASS_ENV_MODEL_CANARY")
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&root);
    assert!(
        output.status.success(),
        "child failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn environment_probe_child() {
    if std::env::var("CAPTUREFAB_MODEL_METADATA_PROBE").as_deref() != Ok("1") {
        return;
    }
    let conn = open_private().unwrap();
    seal(&conn).unwrap();
    let metadata: String = conn
        .query_row(
            "SELECT string_agg(name || '=' || coalesce(value,''),';') FROM duckdb_settings()",
            [],
            |row| row.get(0),
        )
        .unwrap();
    for canary in [
        "HOME_ENV_MODEL_CANARY",
        "USER_ENV_MODEL_CANARY",
        "PASS_ENV_MODEL_CANARY",
        "PRIVATE_ENV_MODEL_CANARY",
        "LOWER_ENV_MODEL_CANARY",
    ] {
        assert!(!metadata.contains(canary), "settings leaked {canary}");
        let transformed: String = conn
            .query_row(
                "SELECT string_agg(reverse(coalesce(value,'')) || hex(coalesce(value,'')),';') FROM duckdb_settings()",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!transformed.contains(&canary.chars().rev().collect::<String>()));
        let encoded = canary
            .as_bytes()
            .iter()
            .map(|byte| format!("{byte:02X}"))
            .collect::<String>();
        assert!(!transformed.contains(&encoded));
    }
    let secrets: i64 = conn
        .query_row("SELECT count(*) FROM duckdb_secrets()", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(secrets, 0);
    let databases: String = conn
        .query_row(
            "SELECT string_agg(coalesce(path,''),';') FROM duckdb_databases()",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!databases.contains("HOME_ENV_MODEL_CANARY"));
    assert!(!databases.contains("source.duckdb"));
    let user: String = conn
        .query_row("SELECT current_user", [], |row| row.get(0))
        .unwrap();
    assert!(!user.contains("USER_ENV_MODEL_CANARY"));
    assert!(
        conn.prepare("SELECT * FROM duckdb_secrets(redact := false)")
            .and_then(|mut statement| statement.query([]).map(|_| ()))
            .is_err()
    );
    let attempts = [
        "SET enable_external_access=true",
        "ATTACH 'must-not-exist.duckdb' AS escaped",
        "COPY (SELECT 1) TO 'must-not-exist.csv'",
        "CREATE PERSISTENT SECRET secret_canary (TYPE HTTP, BEARER_TOKEN 'canary')",
        "INSTALL httpfs",
        "LOAD httpfs",
        "SELECT * FROM read_text('must-not-exist.txt')",
        "SELECT * FROM read_blob('must-not-exist.txt')",
        "SELECT getenv('HTTP_PROXY')",
        "SELECT * FROM read_text('/proc/self/environ')",
    ];
    for sql in attempts {
        // The public guard separately rejects non-SELECT input. Here even a
        // direct SELECT side-effect function cannot bypass the engine boundary.
        assert!(
            conn.execute_batch(sql).is_err(),
            "unsafe SQL succeeded: {sql}"
        );
    }
    assert_eq!(
        conn.query_row("SELECT 1", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        1,
        "ordinary access refusals must leave the engine usable"
    );
    let extension_paths = conn.query_row("SELECT string_agg(reverse(install_path) || hex(install_path),';') FROM duckdb_extensions()", [], |row| row.get::<_, Option<String>>(0));
    if let Ok(Some(paths)) = extension_paths {
        for canary in ["HOME_ENV_MODEL_CANARY", "PRIVATE_ENV_MODEL_CANARY"] {
            assert!(!paths.contains(&canary.chars().rev().collect::<String>()));
            let encoded = canary
                .as_bytes()
                .iter()
                .map(|byte| format!("{byte:02X}"))
                .collect::<String>();
            assert!(!paths.contains(&encoded));
        }
    }
    assert!(!Path::new("must-not-exist.duckdb").exists());
    assert!(!Path::new("must-not-exist.csv").exists());
    assert!(!Path::new(".tmp").exists());
    assert!(!Path::new(INERT_DIRECTORY).exists());
    #[cfg(unix)]
    {
        std::fs::write(":memory:.wal", "SENTINEL_MODEL_CANARY").unwrap();
        for reader in ["read_text", "read_blob", "read_csv_auto"] {
            let sql = format!("SELECT * FROM {reader}(':memory:.wal')");
            assert!(
                conn.prepare(&sql).is_err(),
                "sentinel path allowed: {reader}"
            );
        }
        std::fs::remove_file(":memory:.wal").unwrap();
    }
    let pressure_conn = open_private().unwrap();
    seal(&pressure_conn).unwrap();
    let pressure = pressure_conn
        .execute_batch("SELECT count(DISTINCT repeat(md5(i::VARCHAR),8)) FROM range(500000) t(i)");
    let pressure = pressure.unwrap_err().to_string().to_ascii_lowercase();
    assert!(
        pressure.contains("memory") || pressure.contains("temporary directory"),
        "unexpected pressure failure: {pressure}"
    );
    let temporary: i64 = pressure_conn
        .query_row("SELECT count(*) FROM duckdb_temporary_files()", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(temporary, 0);
    assert!(!Path::new(".tmp").exists());
    // Isolate this intentionally mutating SELECT so a denied profiler output
    // cannot make unrelated access/metadata probes fail for the wrong reason.
    let profiler = open_private().unwrap();
    seal(&profiler).unwrap();
    let _ = profiler.execute_batch("SELECT * FROM enable_profiling(format := 'json', save_location := 'must-not-exist-profile.json')");
    let _ = profiler.execute_batch("SELECT 1");
    assert!(!Path::new("must-not-exist-profile.json").exists());
}
