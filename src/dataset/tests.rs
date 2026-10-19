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

/// First cell of `sql` on a closed dataset, read the way `--file` reads it,
/// so the check also runs in builds that refuse queries in a session.
fn count(path: &std::path::Path, sql: &str) -> Value {
    Dataset::open_read_only(path)
        .unwrap()
        .reader()
        .unwrap()
        .query(sql, 10, 0)
        .unwrap()["rows"][0][0]
        .clone()
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
    let closed = slot.close().unwrap().unwrap();
    assert_eq!(closed.captures, 2);
    assert_eq!(count(&path, "SELECT count(*) FROM captures"), json!(2));
    assert_eq!(
        count(&path, "SELECT count(*) FROM events WHERE kind = 'data_gap'"),
        json!(1)
    );
    assert_eq!(
        count(&path, "SELECT context::VARCHAR FROM captures LIMIT 1"),
        json!(r#"{"line":"L1"}"#)
    );
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
    // Signal samples open it too.
    let samples = dir.join("samples.duckdb");
    slot.set_auto_path(Some(samples.clone()));
    slot.write("signal:0001", &[]).unwrap();
    assert!(!samples.exists());
    slot.write("signal:0001", &[sample(1, "t", Signal::Number(1.0))])
        .unwrap();
    assert_eq!(slot.status_cached().unwrap().samples, 1);
    slot.close().unwrap();
    // Without a dataset, samples are refused (counted as dropped), not
    // reported as written.
    slot.set_auto_path(None);
    let error = slot
        .write("signal:0001", &[sample(2, "t", Signal::Number(1.0))])
        .unwrap_err();
    assert!(error.to_string().contains("no dataset is open"), "{error}");
    // An automatic dataset that cannot be opened says why, and keeps saying
    // so until it is armed again.
    fs::write(dir.join("file"), b"x").unwrap();
    slot.set_auto_path(Some(dir.join("file").join("capturefab.duckdb")));
    slot.record_saved(batch(vec![capture("/x/b.png", 6)]), 0);
    assert!(slot.auto_error().is_some());
    let error = slot
        .write("signal:0001", &[sample(3, "t", Signal::Number(1.0))])
        .unwrap_err();
    assert!(error.to_string().contains("automatic dataset"), "{error}");
    slot.set_auto_path(None);
    assert!(slot.auto_error().is_none());
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn query_guard_sandbox_limits_and_timeout() {
    let dir = temp_dir("query");
    let path = dir.join("q.duckdb");
    let slot = Slot::new();
    slot.open(&path, OpenMode::Create, None).unwrap();
    slot.record_saved(batch(vec![capture("/x/a.png", 1)]), 0);
    let reader = slot.reader().unwrap();
    if !cfg!(feature = "parquet") {
        // No SQL parser check: only a read-only `--file` reader may query.
        let error = reader.query("SELECT 1", 1, 0).err().unwrap();
        assert_eq!(crate::cli::error_code(&error).1, 5, "{error}");
        drop(reader);
        slot.close().unwrap();
        check_reader(
            &dir,
            &Dataset::open_read_only(&path).unwrap().reader().unwrap(),
        );
    } else {
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
        check_reader(&dir, &reader);
        drop(reader);
        slot.close().unwrap();
        assert_eq!(count(&path, "SELECT count(*) FROM captures"), json!(1));
    }
    let _ = fs::remove_dir_all(dir);
}

/// Sandbox, row limit, big integers and timeout of one reader.
fn check_reader(dir: &std::path::Path, reader: &Reader) {
    let error = reader
        .query("SELECT no_such_column FROM captures", 10, 0)
        .err()
        .unwrap();
    assert!(error.is::<Usage>(), "{error}");
    assert_eq!(crate::cli::error_code(&error).1, 2, "{error}");
    let outside = dir.join("outside.csv");
    fs::write(&outside, "a\n1\n").unwrap();
    for sql in [
        format!("SELECT * FROM read_csv('{}')", outside.display()),
        "SELECT * FROM read_text('/etc/hosts')".into(),
    ] {
        // A sandbox refusal is the caller's mistake, not a missing feature.
        let error = reader.query(&sql, 10, 0).err().unwrap();
        assert_eq!(crate::cli::error_code(&error), ("usage", 2), "{error:#}");
    }
    fs::remove_file(&outside).unwrap();
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
    let path = dir.join("s.duckdb");
    let slot = Slot::new();
    slot.open(&path, OpenMode::Create, None).unwrap();
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
    slot.finish().unwrap();
    slot.close().unwrap();
    let dataset = Dataset::open_read_only(&path).unwrap();
    let reader = dataset.reader().unwrap();
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
    drop((reader, dataset));
    // With nothing open the samples are refused, so the hub counts them as
    // dropped rather than written.
    assert!(
        slot.write("x", &[sample(1, "t", Signal::Number(1.0))])
            .is_err()
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
    // An empty dataset already takes about 1 MiB, so 1 MiB could store nothing.
    for (max_bytes, accepted) in [(1 << 20, false), (MIN_MAX_BYTES, true)] {
        let open = C::DatasetOpen {
            path: "/d/p.duckdb".into(),
            mode: OpenMode::Auto,
            max_bytes: Some(max_bytes),
        };
        assert_eq!(ok(open), accepted, "{max_bytes}");
    }
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
    for (id, accepted) in [
        ("0123456789abcdef0123456789abcdef", true),
        ("", false),
        ("foreign/path", false),
    ] {
        assert_eq!(
            ok(C::DatasetCloseIf {
                slot_id: id.into(),
                sink_generation: None
            }),
            accepted
        );
    }
    for (max_result_bytes, accepted) in [
        (0, false),
        (255, false),
        (256, true),
        (8 * 1024 * 1024 + 1, false),
    ] {
        assert_eq!(
            ok(C::DatasetQueryIf {
                slot_id: "0123456789abcdef0123456789abcdef".into(),
                sql: "SELECT 1".into(),
                limit: 1,
                timeout_ms: 1000,
                max_result_bytes
            }),
            accepted
        );
        assert_eq!(
            ok(C::DatasetModelQueryIf {
                slot_id: "0123456789abcdef0123456789abcdef".into(),
                sql: "SELECT 1".into(),
                limit: 1,
                timeout_ms: 1000,
                max_result_bytes
            }),
            accepted
        );
    }
    for (max_bytes, timeout_ms, accepted) in [
        (0, 1000, false),
        (1000, 0, false),
        (1000, 600001, false),
        (1000, 1000, true),
    ] {
        assert_eq!(
            ok(C::DatasetExportIf {
                slot_id: "0123456789abcdef0123456789abcdef".into(),
                output: "/x".into(),
                approved_root: None,
                formats: vec![],
                tables: vec![],
                since_ms: None,
                until_ms: None,
                storage: Default::default(),
                max_bytes,
                timeout_ms
            }),
            accepted
        );
    }
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
    // Capture context (line, part, lot) is summarized for AI tools.
    assert!(summary.contains("- context line: L1 (1)"), "{summary}");
    // The long ASOF table is not loaded under the name `wide`.
    assert!(
        readme.contains("wide = pd.read_csv('captures_wide.csv')") || cfg!(feature = "parquet")
    );
    assert!(readme.contains("joined = pd.read_"), "{readme}");
    assert!(!readme.contains("wide = pd.read_csv('captures_with_signals"));
    assert!(!readme.contains("wide = pd.read_parquet('captures_with_signals"));
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

#[test]
fn checked_dataset_identity_changes_on_reopen_and_preserves_replacement() {
    use crate::session::SessionCommand as C;
    let dir = temp_dir("checked-slot");
    let path = dir.join("data.duckdb");
    let slot = Arc::new(Slot::new());
    let hub = crate::signals::hub::SignalHub::new();
    let open = || C::DatasetOpenIf {
        expected_slot_id: None,
        path: path.to_string_lossy().into_owned(),
        mode: OpenMode::Auto,
        max_bytes: None,
    };
    let first = coordinator::run(&slot, &hub, open()).unwrap();
    let first_id = first["slot_id"].as_str().unwrap().to_owned();
    assert_eq!(first_id.len(), 32);
    let unchanged = slot
        .open_if(Some(&first_id), &path, OpenMode::Open, None)
        .unwrap();
    assert_eq!(unchanged.slot_id.as_deref(), Some(first_id.as_str()));
    coordinator::run(
        &slot,
        &hub,
        C::DatasetCloseIf {
            slot_id: first_id.clone(),
            sink_generation: None,
        },
    )
    .unwrap();
    let second = coordinator::run(&slot, &hub, open()).unwrap();
    let second_id = second["slot_id"].as_str().unwrap().to_owned();
    assert_ne!(first_id, second_id);
    assert_eq!(
        first["dataset_id"], second["dataset_id"],
        "file identity persists"
    );
    assert!(hub.has_sink());
    let auto = dir.join("automatic.duckdb");
    slot.set_auto_path(Some(auto.clone()));
    let events = slot.status_cached().unwrap().events;
    for command in [
        C::DatasetCloseIf {
            slot_id: first_id.clone(),
            sink_generation: None,
        },
        C::DatasetNoteIf {
            slot_id: first_id.clone(),
            text: "stale note".into(),
            context: Context::new(),
            camera: None,
        },
        C::DatasetQueryIf {
            slot_id: first_id.clone(),
            sql: "SELECT 1".into(),
            limit: 1,
            timeout_ms: 1000,
            max_result_bytes: 1000,
        },
        C::DatasetModelQueryIf {
            slot_id: first_id.clone(),
            sql: "SELECT 1".into(),
            limit: 1,
            timeout_ms: 1000,
            max_result_bytes: 1000,
        },
        C::DatasetExportIf {
            slot_id: first_id.clone(),
            output: dir.join("wrong-export").to_string_lossy().into_owned(),
            approved_root: None,
            formats: vec![ExportFormat::Csv],
            tables: vec![],
            since_ms: None,
            until_ms: None,
            storage: Default::default(),
            max_bytes: 100000,
            timeout_ms: 1000,
        },
        open(),
    ] {
        let error = coordinator::run(&slot, &hub, command).unwrap_err();
        assert!(
            error.to_string().contains("dataset slot changed"),
            "{error:#}"
        );
        assert!(
            hub.has_sink(),
            "stale close must not detach the replacement sink"
        );
        assert_eq!(
            slot.auto_path(),
            Some(auto.clone()),
            "stale open/close must not alter automatic setup"
        );
        let status = slot.status_cached().unwrap();
        assert_eq!(status.slot_id.as_deref(), Some(second_id.as_str()));
        assert_eq!(status.events, events);
    }
    assert!(!dir.join("wrong-export").exists());
    let tags = [TagInfo {
        tag: "t".into(),
        unit: Some("°C".into()),
        kind: Some("number".into()),
    }];
    assert!(
        slot.register_for_dataset_slot(&first_id, &source(), &tags)
            .is_err()
    );
    assert!(
        slot.write_for_dataset_slot(
            &first_id,
            "signal:0001",
            &[sample(1, "t", Signal::Number(1.0))]
        )
        .is_err()
    );
    assert_eq!(slot.status_cached().unwrap().signals, 0);
    assert_eq!(slot.status_cached().unwrap().samples, 0);
    slot.register_for_dataset_slot(&second_id, &source(), &tags)
        .unwrap();
    slot.write_for_dataset_slot(
        &second_id,
        "signal:0001",
        &[sample(1, "t", Signal::Number(1.0))],
    )
    .unwrap();
    assert_eq!(slot.status_cached().unwrap().samples, 1);
    coordinator::run(
        &slot,
        &hub,
        C::DatasetCloseIf {
            slot_id: second_id,
            sink_generation: None,
        },
    )
    .unwrap();
    assert!(!hub.has_sink());
    let _ = fs::remove_dir_all(dir);
}

#[test]
#[cfg(feature = "parquet")]
fn bounded_query_rejects_select_sequence_writes_without_advancing_them() {
    let dir = temp_dir("query-sequence-read-only");
    let slot = Slot::new();
    let status = slot
        .open(&dir.join("data.duckdb"), OpenMode::Create, None)
        .unwrap();
    let slot_id = status.slot_id.as_deref().unwrap();
    let reader = slot.reader_if(slot_id).unwrap();
    let sequence = "SELECT last_value FROM duckdb_sequences() WHERE sequence_name='event_ids'";
    let before = reader.query_bounded(sequence, 1, 1000, 512).unwrap()["rows"][0][0].clone();
    for sql in [
        "SELECT nextval('event_ids')",
        "SELECT nextval('event_ids') FROM range(10)",
    ] {
        let error = reader.query_bounded(sql, 10, 1000, 512).unwrap_err();
        assert!(error.to_string().contains("read-only"), "{error:#}");
        assert_eq!(
            reader.query_bounded(sequence, 1, 1000, 512).unwrap()["rows"][0][0],
            before
        );
    }
    let note_id = slot
        .note_if(slot_id, None, "writer remains writable", &Context::new())
        .unwrap();
    assert_eq!(note_id, before.as_u64().unwrap() + 1);
    assert_eq!(
        reader
            .query_bounded("SELECT text FROM events WHERE kind='note'", 1, 1000, 512)
            .unwrap()["rows"][0][0],
        "writer remains writable"
    );
    drop(reader);
    slot.close().unwrap();
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn bounded_query_counts_metadata_and_escaping_and_stays_read_only() {
    let dir = temp_dir("bounded-query");
    let path = dir.join("data.duckdb");
    let slot = Slot::new();
    let status = slot.open(&path, OpenMode::Create, None).unwrap();
    slot.record_event("note", None, None, "a note", &Context::new())
        .unwrap();
    let reader = if cfg!(feature = "parquet") {
        slot.reader_if(status.slot_id.as_deref().unwrap()).unwrap()
    } else {
        slot.close().unwrap();
        let dataset = Dataset::open_read_only(&path).unwrap();
        // Keep its claim alive while using its connection.
        let reader = dataset.reader().unwrap();
        drop(dataset);
        reader
    };
    for cap in [256, 300, 512, 1024, 2048] {
        let result = reader.query_bounded("SELECT range AS n, concat('value,', chr(10), chr(34), 'é') AS value FROM range(10000)", 10000, 1000, cap).unwrap();
        assert!(
            serde_json::to_vec(&result).unwrap().len() <= cap,
            "{result}"
        );
        assert_eq!(result["truncated"], true);
        assert_eq!(result["truncation_reason"], "bytes");
    }
    let huge = reader
        .query_bounded("SELECT repeat('x', 100000) AS text", 10, 1000, 256)
        .unwrap();
    assert_eq!(huge["row_count"], 0);
    assert_eq!(huge["truncation_reason"], "bytes");
    let sql = format!("SELECT 1 AS \"{}\"", "column".repeat(200));
    assert!(
        reader
            .query_bounded(&sql, 10, 1000, 256)
            .unwrap_err()
            .to_string()
            .contains("column metadata exceeds")
    );
    for bad in ["DELETE FROM events", "SELECT 1; DELETE FROM events"] {
        assert!(reader.query_bounded(bad, 1, 1000, 512).is_err());
    }
    assert_eq!(
        reader
            .query_bounded(
                "SELECT count(*) FROM events WHERE kind='note'",
                1,
                1000,
                512
            )
            .unwrap()["rows"][0][0],
        1
    );
    drop(reader);
    slot.close().unwrap();
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn bounded_export_caps_all_files_checksums_and_refuses_overwrite() {
    let dir = temp_dir("bounded-export");
    let slot = Slot::new();
    slot.open(&dir.join("data.duckdb"), OpenMode::Create, None)
        .unwrap();
    slot.record_event(
        "note",
        None,
        None,
        "comma, quote\" newline\nユニコード",
        &Context::new(),
    )
    .unwrap();
    let reader = slot.reader().unwrap();
    let mut formats = vec![ExportFormat::Csv];
    if cfg!(feature = "parquet") {
        formats.extend([
            ExportFormat::Parquet,
            ExportFormat::Json,
            ExportFormat::Jsonl,
        ]);
    }
    let mut request = export_request(dir.join("bundle"), formats);
    request.tables = vec!["events".into()];
    let result = reader.export_bounded(&request, 1_000_000, 10000).unwrap();
    let readme = fs::read_to_string(request.output.join("README.md")).unwrap();
    assert!(readme.contains("events = pd.read_"));
    assert!(!readme.contains("captures = pd.read_"));
    assert!(!readme.contains("samples = pl.read_"));
    let bytes = result["bytes"].as_u64().unwrap();
    let files = result["files"].as_array().unwrap();
    assert_eq!(
        bytes,
        files
            .iter()
            .map(|f| f["bytes"].as_u64().unwrap())
            .sum::<u64>()
    );
    assert!(bytes <= 1_000_000);
    for file in files {
        use sha2::Digest;
        let bytes = fs::read(request.output.join(file["path"].as_str().unwrap())).unwrap();
        assert_eq!(
            format!("{:x}", sha2::Sha256::digest(&bytes)),
            file["sha256"]
        );
    }
    let conn = duckdb::Connection::open_in_memory().unwrap();
    let note: String = conn
        .query_row(
            &format!(
                "SELECT text FROM read_csv('{}', header=true) WHERE kind='note'",
                request.output.join("events.csv").display()
            ),
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(note, "comma, quote\" newline\nユニコード");
    if cfg!(feature = "parquet") {
        let parquet_note: String = conn
            .query_row(
                &format!(
                    "SELECT text FROM read_parquet('{}') WHERE kind='note'",
                    request.output.join("events.parquet").display()
                ),
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(parquet_note, note);
        let rows: Vec<Value> =
            serde_json::from_slice(&fs::read(request.output.join("events.json")).unwrap()).unwrap();
        assert_eq!(
            rows.iter().find(|r| r["kind"] == "note").unwrap()["text"],
            note
        );
        let lines: Vec<Value> = fs::read_to_string(request.output.join("events.jsonl"))
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(rows, lines);
    }
    let before = fs::read(request.output.join("manifest.json")).unwrap();
    assert!(
        reader
            .export_bounded(&request, 1_000_000, 10000)
            .unwrap_err()
            .to_string()
            .contains("already exists")
    );
    assert_eq!(
        fs::read(request.output.join("manifest.json")).unwrap(),
        before
    );
    let before: std::collections::BTreeSet<_> = fs::read_dir(&reader.staging.dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    request.output = dir.join("too-large");
    let error = reader.export_bounded(&request, 200, 10000).unwrap_err();
    assert!(error.to_string().contains("task allowance"), "{error:#}");
    assert!(!request.output.exists());
    let after: std::collections::BTreeSet<_> = fs::read_dir(&reader.staging.dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(
        before, after,
        "partial staged files are removed after a cap failure"
    );
    request.formats = vec![ExportFormat::Parquet];
    if cfg!(feature = "parquet") {
        assert!(
            reader
                .export_bounded(&request, 1, 10000)
                .unwrap_err()
                .to_string()
                .contains("task allowance")
        );
        assert!(!request.output.exists());
    } else {
        assert!(
            reader
                .export_bounded(&request, 100000, 10000)
                .unwrap_err()
                .to_string()
                .contains("unsupported")
        );
    }
    drop(reader);
    slot.close().unwrap();
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn closing_a_slot_cancels_its_checked_reader_before_reuse() {
    let dir = temp_dir("checked-cancel");
    let slot = Arc::new(Slot::new());
    let status = slot
        .open(&dir.join("data.duckdb"), OpenMode::Create, None)
        .unwrap();
    let reader = slot.reader_if(status.slot_id.as_deref().unwrap()).unwrap();
    let closing_slot = slot.clone();
    let closing = std::thread::spawn(move || closing_slot.close().unwrap());
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while reader.check_cancelled().is_ok() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    let error = reader.query_bounded("SELECT 1", 1, 1000, 512).unwrap_err();
    assert!(error.to_string().contains("cancelled"), "{error:#}");
    let request = export_request(dir.join("cancelled-export"), vec![ExportFormat::Csv]);
    assert!(
        reader
            .export_bounded(&request, 100000, 1000)
            .unwrap_err()
            .to_string()
            .contains("cancelled")
    );
    assert!(!request.output.exists());
    drop(reader);
    closing.join().unwrap();
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn bounded_export_preserves_joined_samples_and_time_filters() {
    let dir = temp_dir("bounded-joined");
    let slot = Slot::new();
    slot.open(&dir.join("mixed.duckdb"), OpenMode::Create, None)
        .unwrap();
    slot.register(&source(), &[]).unwrap();
    slot.write(
        "signal:0001",
        &[
            sample(100, "temp", Signal::Number(20.0)),
            sample(200, "temp", Signal::Number(30.0)),
        ],
    )
    .unwrap();
    slot.record_saved(
        batch(vec![capture("/x/one.png", 150), capture("/x/two.png", 250)]),
        0,
    );
    let mut formats = vec![ExportFormat::Csv];
    if cfg!(feature = "parquet") {
        formats.push(ExportFormat::Parquet);
    }
    let mut request = export_request(dir.join("selected"), formats.clone());
    request.tables = vec!["samples".into(), "captures_with_signals".into()];
    request.since_ms = Some(150);
    request.until_ms = Some(250);
    let result = slot
        .reader()
        .unwrap()
        .export_bounded(&request, 1_000_000, 10000)
        .unwrap();
    assert_eq!(result["tables"]["samples"], 1);
    assert_eq!(result["tables"]["captures_with_signals"], 2);
    let conn = duckdb::Connection::open_in_memory().unwrap();
    for format in formats {
        let ext = format.extension();
        let rows: Vec<(u64, f64)> = conn
            .prepare(&format!(
                "SELECT sample_ms, value_double FROM '{}' ORDER BY capture_id",
                request
                    .output
                    .join(format!("captures_with_signals.{ext}"))
                    .display()
            ))
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<duckdb::Result<_>>()
            .unwrap();
        assert_eq!(rows, [(100, 20.0), (200, 30.0)]);
        let rows: u64 = conn
            .query_row(
                &format!(
                    "SELECT count(*) FROM '{}' WHERE ts_ms = 200",
                    request.output.join(format!("samples.{ext}")).display()
                ),
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rows, 1);
        assert!(!request.output.join(format!("captures.{ext}")).exists());
    }
    let manifest: Value =
        serde_json::from_slice(&fs::read(request.output.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["filters"]["since_ms"], 150);
    assert_eq!(manifest["filters"]["until_ms"], 250);
    assert_eq!(manifest["slot_id"], result["slot_id"]);
    slot.close().unwrap();
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn bounded_export_enforces_the_pinned_capture_root() {
    let dir = temp_dir("export-root");
    let allowed = dir.join("allowed");
    let outside = dir.join("outside");
    fs::create_dir_all(&allowed).unwrap();
    fs::create_dir_all(&outside).unwrap();
    let slot = Slot::new();
    slot.open(&dir.join("data.duckdb"), OpenMode::Create, None)
        .unwrap();
    let reader = slot.reader().unwrap();
    let mut request = export_request(outside.join("bundle"), vec![ExportFormat::Csv]);
    request.tables = vec!["events".into()];
    let error = reader
        .export_bounded_in(&request, 100000, 10000, Some(&allowed))
        .unwrap_err();
    assert!(
        error.to_string().contains("approved capture root"),
        "{error:#}"
    );
    assert!(!request.output.exists());
    request.output = allowed.join("new-parent/bundle");
    let result = reader
        .export_bounded_in(&request, 100000, 10000, Some(&allowed))
        .unwrap();
    assert!(request.output.join("events.csv").is_file());
    assert!(result["bytes"].as_u64().unwrap() <= 100000);
    drop(reader);
    slot.close().unwrap();
    let _ = fs::remove_dir_all(dir);
}
