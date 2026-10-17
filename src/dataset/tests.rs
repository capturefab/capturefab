use super::*;
use crate::signals::{Protocol, Quality, Sample, SampleSink, SignalInfo, TagInfo, Value as Signal};
use std::{fs, path::PathBuf};

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "capturefab-dataset-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

fn camera() -> SavedCamera {
    SavedCamera {
        id: "sim:0".into(),
        transport: "simulator".into(),
        vendor: "Capturefab".into(),
        model: "Simulator".into(),
        serial: "SIM0".into(),
        source: None,
    }
}

fn capture(path: &str, wall_ms: u64) -> SavedFile {
    SavedFile {
        seq: 0,
        wall_ms,
        path: path.into(),
        bytes: 100,
        sha256: Some("ab".repeat(32)),
        destination: None,
        object_key: None,
        remote_url: None,
        upload_id: None,
        context: [("line".to_string(), "L1".to_string())].into(),
        item: SavedItem::Capture {
            frame_id: 1,
            timestamp_ns: 2,
            width: 640,
            height: 480,
            pixel_format: 0x0108_0001,
            format: "png".into(),
            origin: "capture".into(),
            job_id: None,
        },
    }
}

fn batch(items: Vec<SavedFile>) -> SavedBatch {
    SavedBatch {
        camera: Some(camera()),
        items,
        lost: 0,
    }
}

fn count(slot: &Slot, sql: &str) -> Value {
    slot.reader().unwrap().query(sql, 10, 0).unwrap()["rows"][0][0].clone()
}

fn source() -> SignalInfo {
    SignalInfo {
        id: "signal:0001".into(),
        protocol: Protocol::SimSignal,
        uri: "sim-signal://line1".into(),
        interval_ms: 100,
    }
}

fn sample(ts_ms: u64, tag: &str, value: Signal) -> Sample {
    Sample {
        ts_ms,
        source_ts_ms: None,
        tag: tag.into(),
        value,
        quality: Quality::Good,
    }
}

#[test]
fn create_refuses_existing_and_open_refuses_missing() {
    let dir = temp_dir("modes");
    let path = dir.join("plant.duckdb");
    let error = Dataset::open(&path, OpenMode::Open, None).err().unwrap();
    assert!(error.to_string().contains("does not exist"), "{error}");
    assert!(!path.exists());
    let created = Dataset::open(&path, OpenMode::Create, None).unwrap();
    assert_eq!(created.status().name, "plant");
    drop(created);
    let error = Dataset::open(&path, OpenMode::Create, None).err().unwrap();
    assert!(error.to_string().contains("already exists"), "{error}");
    let reopened = Dataset::open(&path, OpenMode::Auto, None).unwrap();
    assert_eq!(reopened.status().events, 2);
    let error = Dataset::open(&dir.join("x.db"), OpenMode::Auto, None)
        .err()
        .unwrap();
    assert!(error.to_string().contains(".duckdb"), "{error}");
    drop(reopened);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn open_refuses_empty_and_foreign_files_without_changing_them() {
    let dir = temp_dir("foreign");
    let empty = dir.join("empty.duckdb");
    fs::write(&empty, b"").unwrap();
    let error = Dataset::open(&empty, OpenMode::Auto, None).err().unwrap();
    assert!(
        error.to_string().contains("not a Capturefab dataset"),
        "{error}"
    );
    assert_eq!(fs::read(&empty).unwrap(), b"");
    let foreign = dir.join("other.duckdb");
    {
        let conn = duckdb::Connection::open(&foreign).unwrap();
        conn.execute_batch("CREATE TABLE t (x INTEGER); INSERT INTO t VALUES (1); CHECKPOINT;")
            .unwrap();
    }
    let before = fs::read(&foreign).unwrap();
    let error = Dataset::open(&foreign, OpenMode::Open, None).err().unwrap();
    assert!(
        error.to_string().contains("not a Capturefab dataset"),
        "{error}"
    );
    assert_eq!(fs::read(&foreign).unwrap(), before);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn in_process_double_open_reports_locked() {
    let dir = temp_dir("double");
    let path = dir.join("d.duckdb");
    let first = Dataset::open(&path, OpenMode::Auto, None).unwrap();
    let error = Dataset::open_read_only(&path).err().unwrap();
    assert!(
        crate::cli::error_code(&error).1 == 3 && error.to_string().contains("--session"),
        "{error}"
    );
    drop(first);
    assert!(Dataset::open_read_only(&path).is_ok());
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn slot_records_saved_files_once_and_counts_gaps() {
    let dir = temp_dir("saved");
    let slot = Slot::new();
    assert!(slot.status_cached().is_none());
    // Nothing open: only the recent list changes.
    slot.record_saved(batch(vec![capture("/x/a.png", 1)]), 0);
    assert_eq!(slot.recent().len(), 1);
    let path = dir.join("d.duckdb");
    slot.open(&path, OpenMode::Auto, None).unwrap();
    let error = slot
        .open(&dir.join("other.duckdb"), OpenMode::Auto, None)
        .err()
        .unwrap();
    assert!(error.to_string().contains("close it first"), "{error}");
    assert_eq!(slot.open(&path, OpenMode::Auto, None).unwrap().captures, 0);
    let items = vec![capture("/x/a.png", 10), capture("/x/b.png", 11)];
    slot.record_saved(batch(items.clone()), 0);
    slot.record_saved(batch(items), 2);
    let status = slot.status_cached().unwrap();
    assert_eq!((status.captures, status.missed_files), (2, 2));
    assert_eq!(status.last_saved.as_deref(), Some("/x/b.png"));
    assert_eq!(count(&slot, "SELECT count(*) FROM captures"), json!(2));
    assert_eq!(
        count(&slot, "SELECT count(*) FROM events WHERE kind = 'data_gap'"),
        json!(1)
    );
    assert_eq!(
        count(&slot, "SELECT context::VARCHAR FROM captures LIMIT 1"),
        json!(r#"{"line":"L1"}"#)
    );
    let closed = slot.close().unwrap().unwrap();
    assert_eq!(closed.captures, 2);
    assert!(slot.status_cached().is_none() && slot.close().unwrap().is_none());
    // Reopening keeps the counts.
    assert_eq!(slot.open(&path, OpenMode::Open, None).unwrap().captures, 2);
    slot.close().unwrap();
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn auto_path_opens_lazily_on_first_saved_file() {
    let dir = temp_dir("auto");
    let slot = Slot::new();
    let path = dir.join("nested").join("capturefab.duckdb");
    slot.set_auto_path(Some(path.clone()));
    slot.record_saved(SavedBatch::default(), 0);
    assert!(!path.exists());
    slot.record_saved(batch(vec![capture("/x/a.png", 5)]), 0);
    assert_eq!(slot.status_cached().unwrap().captures, 1);
    slot.close().unwrap();
    assert!(path.exists());
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn query_guard_sandbox_limits_and_timeout() {
    let dir = temp_dir("query");
    let slot = Slot::new();
    slot.open(&dir.join("q.duckdb"), OpenMode::Create, None)
        .unwrap();
    slot.record_saved(batch(vec![capture("/x/a.png", 1)]), 0);
    let reader = slot.reader().unwrap();
    if cfg!(feature = "parquet") {
        for bad in [
            "DELETE FROM captures; SELECT 1",
            "SELECT 1; SELECT 2",
            "DELETE FROM captures",
            "DROP TABLE captures",
            "COPY captures TO '/tmp/x.csv'",
            "ATTACH '/tmp/x.duckdb'",
            "SET enable_external_access = true",
        ] {
            let error = reader.query(bad, 10, 0).err().unwrap();
            assert!(error.is::<Usage>(), "{bad}: {error}");
        }
        assert_eq!(count(&slot, "SELECT count(*) FROM captures"), json!(1));
    }
    let outside = dir.join("outside.csv");
    let error = reader
        .query(
            &format!("SELECT * FROM read_csv('{}')", outside.display()),
            10,
            0,
        )
        .err();
    assert!(error.is_some() && !outside.exists());
    let result = reader
        .query(
            "SELECT range AS n, 9007199254740993::UBIGINT AS big FROM range(5)",
            3,
            0,
        )
        .unwrap();
    assert_eq!(result["row_count"], 3);
    assert_eq!(result["truncated"], true);
    assert_eq!(result["rows"][0][1], json!("9007199254740993"));
    assert_eq!(result["columns"][0]["name"], "n");
    let started = std::time::Instant::now();
    let error = reader
        .query("SELECT count(*) FROM range(1000000000000) a", 1, 200)
        .err()
        .unwrap();
    assert!(error.to_string().contains("timed out"), "{error}");
    assert_eq!(crate::cli::error_code(&error).1, 4);
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
    drop(reader);
    slot.close().unwrap();
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn reader_does_not_block_writer_and_close_interrupts_readers() {
    let dir = temp_dir("readers");
    let slot = std::sync::Arc::new(Slot::new());
    slot.open(&dir.join("r.duckdb"), OpenMode::Create, None)
        .unwrap();
    let reader = slot.reader().unwrap();
    let busy = std::thread::spawn(move || {
        reader.query("SELECT count(*) FROM range(1000000000000) a", 1, 60_000)
    });
    std::thread::sleep(std::time::Duration::from_millis(300));
    let started = std::time::Instant::now();
    slot.record_saved(batch(vec![capture("/x/a.png", 1)]), 0);
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
    assert_eq!(slot.status_cached().unwrap().captures, 1);
    slot.close().unwrap();
    assert!(busy.join().unwrap().is_err());
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn samples_signals_notes_and_asof_view() {
    let dir = temp_dir("signals");
    let slot = Slot::new();
    slot.open(&dir.join("s.duckdb"), OpenMode::Create, None)
        .unwrap();
    // A sink with nothing open accepts writes silently.
    let tags = [TagInfo {
        tag: "temp".into(),
        unit: Some("degC".into()),
        kind: Some("number".into()),
    }];
    slot.register(&source(), &tags).unwrap();
    slot.write(
        "signal:0001",
        &[
            sample(100, "temp", Signal::Number(20.0)),
            sample(200, "temp", Signal::Number(21.5)),
            sample(300, "temp", Signal::Number(30.0)),
            sample(150, "running", Signal::Bool(true)),
        ],
    )
    .unwrap();
    slot.event("signal_gap", "signal:0001", "helper restarted")
        .unwrap();
    assert_eq!(
        slot.record_event("note", None, None, "shift B", &Context::new())
            .unwrap()
            .map(|id| id > 0),
        Some(true)
    );
    slot.record_saved(batch(vec![capture("/x/a.png", 250)]), 0);
    let status = slot.status_cached().unwrap();
    assert_eq!((status.samples, status.signals), (4, 2));
    let reader = slot.reader().unwrap();
    let rows = reader
        .query(
            "SELECT tag, value_double, sample_age_ms, unit FROM captures_with_signals ORDER BY tag",
            10,
            0,
        )
        .unwrap();
    assert_eq!(
        rows["rows"],
        json!([["running", 1.0, 100, null], ["temp", 21.5, 50, "degC"]])
    );
    let signal = reader
        .query(
            "SELECT first_ms, last_ms, protocol, kind FROM signals WHERE tag = 'temp'",
            1,
            0,
        )
        .unwrap();
    assert_eq!(signal["rows"][0], json!([100, 300, "sim-signal", "number"]));
    drop(reader);
    slot.finish().unwrap();
    slot.close().unwrap();
    assert!(
        slot.write("x", &[sample(1, "t", Signal::Number(1.0))])
            .is_ok()
    );
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn size_cap_reports_storage_full() {
    let dir = temp_dir("cap");
    let slot = Slot::new();
    slot.open(&dir.join("c.duckdb"), OpenMode::Create, Some(1024 * 1024))
        .unwrap();
    slot.record_saved(batch(vec![capture("/x/a.png", 1)]), 0);
    let status = slot.status_cached().unwrap();
    assert_eq!(status.captures, 0);
    assert_eq!(status.missed_files, 1);
    let error = status.last_error.unwrap();
    assert!(error.contains("Storage full"), "{error}");
    slot.close().unwrap();
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn redacts_source_credentials() {
    let info = crate::types::CameraInfo {
        id: "rtsp:cam".into(),
        transport: crate::types::Transport::Simulator,
        vendor: String::new(),
        model: String::new(),
        serial: String::new(),
        address: Some("rtsp://user:secret@10.0.0.2/stream".into()),
        profile: Default::default(),
    };
    let camera = SavedCamera::from(&info);
    assert!(!camera.source.unwrap().contains("secret"));
}

#[test]
fn validate_command_bounds() {
    use crate::session::SessionCommand as C;
    let ok = |c: C| validate_command(&c).is_ok();
    assert!(ok(C::DatasetOpen {
        path: "/d/p.duckdb".into(),
        mode: OpenMode::Auto,
        max_bytes: None
    }));
    assert!(!ok(C::DatasetOpen {
        path: "/d/p.db".into(),
        mode: OpenMode::Auto,
        max_bytes: None
    }));
    assert!(!ok(C::DatasetNote {
        text: String::new(),
        context: Context::new(),
        camera: None
    }));
    assert!(ok(C::DatasetNote {
        text: "line\tone\ntwo".into(),
        context: Context::new(),
        camera: None
    }));
    assert!(!ok(C::DatasetQuery {
        sql: "SELECT 1".into(),
        limit: 0,
        timeout_ms: 0
    }));
    assert!(!ok(C::DatasetExport {
        output: "/x".into(),
        formats: vec![],
        tables: vec!["nope".into()],
        since_ms: None,
        until_ms: None,
        storage: Default::default()
    }));
    assert!(!ok(C::DatasetExport {
        output: "/x".into(),
        formats: vec![],
        tables: vec![],
        since_ms: Some(2),
        until_ms: Some(1),
        storage: Default::default()
    }));
}

fn export_request(output: PathBuf, formats: Vec<ExportFormat>) -> export::ExportRequest {
    export::ExportRequest {
        output,
        formats,
        tables: vec![],
        since_ms: None,
        until_ms: None,
        storage: Default::default(),
    }
}

#[test]
fn export_bundle_writes_every_format_once_and_never_overwrites() {
    let dir = temp_dir("export");
    let slot = Slot::new();
    slot.open(&dir.join("e.duckdb"), OpenMode::Create, None)
        .unwrap();
    slot.register(&source(), &[]).unwrap();
    slot.write(
        "signal:0001",
        &[
            sample(100, "temp", Signal::Number(20.0)),
            sample(100, "it's", Signal::Number(1.0)),
        ],
    )
    .unwrap();
    slot.record_saved(batch(vec![capture("/x/a.png", 150)]), 0);
    slot.record_event("note", None, None, "first \"note\"", &Context::new())
        .unwrap();
    let output = dir.join("bundle");
    let formats = if cfg!(feature = "parquet") {
        vec![
            ExportFormat::Csv,
            ExportFormat::Parquet,
            ExportFormat::Json,
            ExportFormat::Jsonl,
        ]
    } else {
        vec![ExportFormat::Csv]
    };
    let result = slot
        .reader()
        .unwrap()
        .export(&export_request(output.clone(), formats.clone()))
        .unwrap();
    assert_eq!(result["tables"]["captures"], 1);
    assert_eq!(result["tables"]["captures_wide"], 1);
    for table in TABLES {
        for format in &formats {
            assert!(
                output
                    .join(format!("{table}.{}", format.extension()))
                    .is_file(),
                "{table} {format:?}"
            );
        }
    }
    let manifest: Value =
        serde_json::from_slice(&fs::read(output.join("manifest.json")).unwrap()).unwrap();
    for table in manifest["tables"].as_array().unwrap() {
        for file in table["files"].as_array().unwrap() {
            let bytes = fs::read(output.join(file["path"].as_str().unwrap())).unwrap();
            use sha2::Digest;
            assert_eq!(
                format!("{:x}", sha2::Sha256::digest(&bytes)),
                file["sha256"].as_str().unwrap()
            );
        }
    }
    let wide = fs::read_to_string(output.join("captures_wide.csv")).unwrap();
    assert!(wide.contains("temp") && wide.contains("it's"), "{wide}");
    let readme = fs::read_to_string(output.join("README.md")).unwrap();
    let summary = &readme[readme.find("## Summary for AI tools").unwrap()..];
    assert!(summary.len() <= export::SUMMARY_BYTES && summary.contains("first 'note'"));
    if cfg!(feature = "parquet") {
        let check = Dataset::open_read_only(&dir.join("e.duckdb")).err();
        assert!(check.is_some(), "the session still holds the dataset");
        let conn = duckdb::Connection::open_in_memory().unwrap();
        let rows: u64 = conn
            .query_row(
                &format!(
                    "SELECT count(*) FROM '{}'",
                    output.join("samples.parquet").display()
                ),
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rows, 2);
    }
    let before = fs::read(output.join("manifest.json")).unwrap();
    let error = slot
        .reader()
        .unwrap()
        .export(&export_request(output.clone(), vec![ExportFormat::Csv]))
        .err()
        .unwrap();
    assert!(error.to_string().contains("already exists"), "{error}");
    assert_eq!(fs::read(output.join("manifest.json")).unwrap(), before);
    slot.close().unwrap();
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn export_quota_preflight_reports_storage_full() {
    let dir = temp_dir("quota");
    let slot = Slot::new();
    slot.open(&dir.join("q.duckdb"), OpenMode::Create, None)
        .unwrap();
    slot.record_saved(batch(vec![capture("/x/a.png", 1)]), 0);
    let mut request = export_request(dir.join("out"), vec![ExportFormat::Csv]);
    request.storage.max_bytes = 1;
    let error = slot.reader().unwrap().export(&request).err().unwrap();
    assert_eq!(crate::cli::error_code(&error).1, 6, "{error}");
    assert!(!dir.join("out").exists());
    slot.close().unwrap();
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn unsupported_formats_without_parquet() {
    if cfg!(feature = "parquet") {
        return;
    }
    let dir = temp_dir("noparquet");
    let slot = Slot::new();
    slot.open(&dir.join("n.duckdb"), OpenMode::Create, None)
        .unwrap();
    let error = slot
        .reader()
        .unwrap()
        .export(&export_request(dir.join("o"), vec![ExportFormat::Parquet]))
        .err()
        .unwrap();
    assert_eq!(crate::cli::error_code(&error).1, 5, "{error}");
    slot.close().unwrap();
    let _ = fs::remove_dir_all(dir);
}
