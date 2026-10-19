//! Fresh task query contracts. Fixture-only SQL seeds camera metadata which
//! the typed signal writer does not create; no production connection escapes.
use super::*;
use crate::signals::{Protocol, Quality, Sample, SignalInfo, Value};
use std::sync::atomic::AtomicBool;
#[cfg(feature = "parquet")]
use std::{
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

struct Fixture {
    root: PathBuf,
    budget: Arc<TaskExtentBudget>,
    data: Option<TaskDataset>,
    source: SignalInfo,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "capturefab-task-model-{}-{}",
            std::process::id(),
            super::super::db::random_name().unwrap()
        ));
        crate::ipc::ensure_private_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let budget = TaskExtentBudget::new(32 * 1024 * 1024).unwrap();
        let mut data = TaskDataset::create(&root, budget.clone()).unwrap();
        data.initialize_catalog("private task query").unwrap();
        // Deliberately seed raw locator metadata to prove the projection also
        // protects historical catalogs, rather than relying on input redaction.
        let uri = "mqtt://user:TASK_MODEL_PASSWORD@example.test/private-task-topic?client_id=TASK_MODEL_CLIENT";
        let source = SignalInfo {
            id: crate::signals::source_id(uri),
            protocol: Protocol::Mqtt,
            uri: uri.into(),
            interval_ms: 100,
        };
        data.register(&source, &[]).unwrap();
        data.append_samples(
            &source.id,
            &[
                reading(1400, "temperature", Value::Number(42.25)),
                reading(1401, "state", Value::Text("authorized task text".into())),
            ],
        )
        .unwrap();
        data.record_event(
            "note",
            None,
            Some(&source.id),
            "authorized task note",
            &crate::context::Context::new(),
        )
        .unwrap();
        #[cfg(feature = "parquet")]
        {
            let home = std::env::var("HOME").unwrap();
            data.connection().unwrap().execute(
                "INSERT INTO captures(wall_ms,camera_id,serial,source,format,bytes,path,file_name,origin) VALUES(1500,'sim:0','TASK_MODEL_SERIAL',?,'png',1,?,'consented.png','capture')",
                [&source.uri, &format!("{home}/consented.png")],
            ).unwrap();
        }
        data.checkpoint_catalog().unwrap();
        Self {
            root,
            budget,
            data: Some(data),
            source,
        }
    }
    fn data(&mut self) -> &mut TaskDataset {
        self.data.as_mut().unwrap()
    }
    fn append(&mut self, timestamp: u64) {
        let source = self.source.id.clone();
        self.data()
            .append_samples(
                &source,
                &[reading(timestamp, "temperature", Value::Number(3.0))],
            )
            .unwrap();
    }
    #[cfg(feature = "parquet")]
    fn query(&mut self, sql: &str) -> Result<serde_json::Value> {
        self.data()
            .query_model_bounded(sql, 100, 3000, 32 * 1024, &AtomicBool::new(false))
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(data) = self.data.take() {
            data.remove().unwrap();
        }
        assert_eq!(self.budget.usage().unwrap().used, 0);
        std::fs::remove_dir_all(&self.root).unwrap();
    }
}
fn reading(ts_ms: u64, tag: &str, value: Value) -> Sample {
    Sample {
        ts_ms,
        source_ts_ms: None,
        tag: tag.into(),
        value,
        quality: Quality::Good,
    }
}

#[cfg(feature = "parquet")]
fn limits() -> super::super::model_snapshot::ModelQueryLimits {
    super::super::model_snapshot::ModelQueryLimits::bounded(100, 3000, 32 * 1024).unwrap()
}

#[cfg(feature = "parquet")]
fn assert_no_transform(reply: &serde_json::Value, secrets: &[&str]) {
    let text = reply.to_string();
    for secret in secrets {
        for encoded in [
            secret.to_string(),
            secret.chars().rev().collect::<String>(),
            secret
                .as_bytes()
                .iter()
                .map(|byte| format!("{byte:02X}"))
                .collect::<String>(),
        ] {
            assert!(
                !text.contains(&encoded),
                "private metadata survived a SQL transform"
            );
        }
    }
}

#[cfg(feature = "parquet")]
#[test]
fn fresh_task_queries_project_before_sql_and_preserve_typed_values_and_raw_catalog() {
    let mut fixture = Fixture::new();
    let identity = fixture.data().catalog_identity().unwrap().clone();
    let path = fixture.data().path().to_string_lossy().into_owned();
    let usage = fixture.budget.usage().unwrap();
    let samples = fixture
        .query("SELECT tag,value_double,value_text FROM samples ORDER BY tag")
        .unwrap();
    assert_eq!(
        samples["rows"][0],
        serde_json::json!(["state", null, "authorized task text"])
    );
    assert_eq!(
        samples["rows"][1],
        serde_json::json!(["temperature", 42.25, null])
    );
    let joined = fixture
        .query(
            "SELECT camera_id,tag,value_double,value_text FROM captures_with_signals ORDER BY tag",
        )
        .unwrap();
    assert_eq!(
        joined["rows"][1],
        serde_json::json!(["sim:0", "temperature", 42.25, null])
    );
    let notes = fixture.query("SELECT text FROM events").unwrap();
    assert_eq!(notes["rows"][0][0], "authorized task note");
    let protected = fixture.query("SELECT serial,replace(serial,'TASK_',''),reverse(source),hex(path) FROM captures UNION ALL SELECT NULL,uri,reverse(uri),hex(uri) FROM signals").unwrap();
    assert!(protected["rows"][0][0].is_null());
    let home = std::env::var("HOME").unwrap();
    assert_no_transform(
        &protected,
        &[
            "TASK_MODEL_SERIAL",
            "TASK_MODEL_PASSWORD",
            "TASK_MODEL_CLIENT",
            "private-task-topic",
            &home,
        ],
    );
    let metadata = fixture.query("SELECT name,reverse(value),hex(value) FROM duckdb_settings() WHERE name IN ('temp_directory','allowed_paths','allowed_directories','http_proxy','http_proxy_username','http_proxy_password','home_directory','secret_directory','extension_directory','username','password')").unwrap();
    assert_no_transform(&metadata, &[&path, &home, "TASK_MODEL_PASSWORD"]);
    let settings = fixture.query("SELECT current_setting('temp_directory'),current_setting('max_temp_directory_size'),current_setting('enable_external_access'),current_setting('lock_configuration'),current_setting('http_proxy')").unwrap();
    assert_eq!(
        settings["rows"][0],
        serde_json::json!(["", "0 bytes", false, true, ""])
    );
    let raw: (String, String) = fixture
        .data()
        .connection()
        .unwrap()
        .query_row("SELECT serial,source FROM captures", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .unwrap();
    assert_eq!(
        raw,
        ("TASK_MODEL_SERIAL".into(), fixture.source.uri.clone())
    );
    assert_eq!(fixture.data().catalog_identity().unwrap(), &identity);
    let after = fixture.budget.usage().unwrap();
    assert_eq!(
        (after.used, after.peak, after.spill_opens),
        (usage.used, usage.peak, usage.spill_opens)
    );
    fixture.append(1600);
    assert_eq!(fixture.data().catalog_status().unwrap().dataset.samples, 3);
}

#[cfg(feature = "parquet")]
#[test]
fn fresh_task_copy_caps_and_result_caps_never_fall_back_or_poison_the_writer() {
    let mut fixture = Fixture::new();
    let reply = fixture
        .data()
        .query_model_bounded(
            "SELECT i,repeat('x',100) FROM range(100) t(i)",
            2,
            3000,
            512,
            &AtomicBool::new(false),
        )
        .unwrap();
    assert!(serde_json::to_vec(&reply).unwrap().len() <= 512);
    assert!(reply["rows"].as_array().unwrap().len() <= 2);
    assert_eq!(reply["truncated"], true);
    for snapshot in [
        SnapshotLimits {
            max_rows: 1,
            ..limits().snapshot
        },
        SnapshotLimits {
            max_arrow_bytes: 1,
            ..limits().snapshot
        },
        SnapshotLimits {
            max_batch_bytes: 1,
            ..limits().snapshot
        },
    ] {
        let bounds = super::super::model_snapshot::ModelQueryLimits {
            snapshot,
            ..limits()
        };
        let error = fixture
            .data()
            .query_model_with_limits(
                "SELECT serial FROM captures",
                bounds,
                &AtomicBool::new(false),
            )
            .unwrap_err();
        assert!(error.to_string().contains("limit"), "{error:#}");
        assert!(!error.to_string().contains("TASK_MODEL_SERIAL"));
        fixture.append(1700);
    }
    assert_eq!(fixture.data().catalog_status().unwrap().dataset.samples, 5);
    assert!(!fixture.data().catalog_status().unwrap().quarantined);
    assert_eq!(
        fixture.query("SELECT count(*) FROM samples").unwrap()["rows"][0][0],
        5
    );
}

#[cfg(feature = "parquet")]
#[test]
fn fresh_task_cancellation_and_overall_deadline_leave_source_writable() {
    let mut fixture = Fixture::new();
    assert!(
        fixture
            .data()
            .query_model_bounded("SELECT 1", 1, 1000, 1024, &AtomicBool::new(true))
            .unwrap_err()
            .to_string()
            .contains("cancelled")
    );
    let cancel = AtomicBool::new(false);
    let started = Instant::now();
    let result = std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(100));
            cancel.store(true, Ordering::Release);
        });
        fixture.data().query_model_bounded(
            "SELECT sum(sin(i::DOUBLE)) FROM range(100000000000) t(i)",
            1,
            3000,
            1024,
            &cancel,
        )
    });
    let error = result.unwrap_err();
    assert!(error.to_string().contains("cancelled"), "{error:#}");
    assert!(started.elapsed() < Duration::from_secs(3));
    fixture.append(1800);
    let started = Instant::now();
    let error = fixture
        .data()
        .query_model_bounded(
            "SELECT sum(sin(i::DOUBLE)) FROM range(100000000000) t(i)",
            1,
            1,
            1024,
            &AtomicBool::new(false),
        )
        .unwrap_err();
    assert!(error.to_string().contains("deadline"), "{error:#}");
    assert!(started.elapsed() < Duration::from_secs(1));
    fixture.append(1900);
    assert_eq!(
        fixture.query("SELECT count(*) FROM samples").unwrap()["rows"][0][0],
        4
    );
    assert!(!fixture.data().catalog_status().unwrap().quarantined);
}

#[cfg(feature = "parquet")]
#[test]
fn fresh_task_schema_code_and_generated_metadata_are_rejected_before_any_scan() {
    let mut fixture = Fixture::new();
    fixture.data().connection().unwrap().execute_batch("CREATE MACRO main.current_database() AS 'TASK_MODEL_MACRO'; CREATE MACRO main.duckdb_tables() AS TABLE SELECT 'TASK_MODEL_MACRO' AS sql; CREATE MACRO main.duckdb_columns() AS TABLE SELECT 'TASK_MODEL_MACRO' AS column_name").unwrap();
    assert_eq!(
        fixture.query("SELECT count(*) FROM samples").unwrap()["rows"][0][0],
        2
    );
    fixture.data().connection().unwrap().execute_batch("ALTER TABLE samples RENAME TO original_samples; CREATE TABLE samples(ts_ms UBIGINT NOT NULL,source_ts_ms UBIGINT,source_id VARCHAR NOT NULL,tag VARCHAR NOT NULL,value_double DOUBLE,value_text VARCHAR GENERATED ALWAYS AS (current_setting('temp_directory')),quality VARCHAR); INSERT INTO samples(ts_ms,source_id,tag,value_double,quality) VALUES(1400,'sensor','temperature',42.25,'good')").unwrap();
    let error = fixture
        .query("SELECT reverse(value_text),hex(value_text) FROM samples")
        .unwrap_err();
    assert!(error.to_string().contains("schema"), "{error:#}");
    assert!(!error.to_string().contains("TASK_MODEL_MACRO"));
    assert!(
        !error
            .to_string()
            .contains(fixture.data().path().to_str().unwrap())
    );
}

#[cfg(feature = "parquet")]
#[test]
fn fresh_task_arbitrary_sql_cannot_open_source_paths_or_spill_into_the_native_lease() {
    let mut fixture = Fixture::new();
    let path = super::super::db::sql_text(fixture.data().database_path().to_str().unwrap());
    let usage = fixture.budget.usage().unwrap();
    for sql in [
        format!("SELECT * FROM read_blob('{path}')"),
        "SELECT * FROM read_text(':memory:.wal')".into(),
        "SET enable_external_access=true".into(),
        "COPY samples TO 'task-model-output.csv'".into(),
        "SELECT nextval('event_ids')".into(),
    ] {
        assert!(
            fixture.query(&sql).is_err(),
            "unsafe model statement succeeded"
        );
    }
    assert_eq!(fixture.query("SELECT 1").unwrap()["rows"][0][0], 1);
    let error = fixture
        .query("SELECT count(DISTINCT repeat(md5(i::VARCHAR),8)) FROM range(2000000) t(i)")
        .unwrap_err();
    let message = format!("{error:#}").to_lowercase();
    assert!(
        message.contains("out of memory") || message.contains("no temporary directory"),
        "{error:#}"
    );
    let after = fixture.budget.usage().unwrap();
    assert_eq!(
        (after.used, after.peak, after.spill_opens),
        (usage.used, usage.peak, usage.spill_opens)
    );
    fixture.append(2000);
}

#[cfg(not(feature = "parquet"))]
#[test]
fn fresh_task_model_query_without_parser_fails_closed_and_keeps_catalog_writable() {
    let mut fixture = Fixture::new();
    let error = fixture
        .data()
        .query_model_bounded("SELECT 42", 1, 1000, 1024, &AtomicBool::new(false))
        .unwrap_err();
    assert!(error.to_string().contains("unsupported"), "{error:#}");
    fixture.append(2000);
    assert_eq!(fixture.data().catalog_status().unwrap().dataset.samples, 3);
}

#[test]
fn fresh_task_model_query_refuses_a_closed_catalog() {
    let mut fixture = Fixture::new();
    fixture.data().close().unwrap();
    let error = fixture
        .data()
        .query_model_bounded("SELECT 1", 1, 1000, 1024, &AtomicBool::new(false))
        .unwrap_err();
    assert!(error.to_string().contains("closed"), "{error:#}");
}
