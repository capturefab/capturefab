//! Task-owned wrappers around the shared bounded snapshot copier.
use super::{TaskDataset, TaskMode};
use crate::dataset::Reader;
use crate::dataset::snapshot::{self, SnapshotLimits, SnapshotProjection, SnapshotReceipt};
use anyhow::{Result, ensure};
#[cfg(all(test, unix, feature = "parquet"))]
use std::sync::{atomic::Ordering, mpsc};
use std::{
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};

impl TaskDataset {
    pub fn copy_snapshot(
        &mut self,
        source: &Reader,
        limits: SnapshotLimits,
        cancel: &AtomicBool,
    ) -> Result<SnapshotReceipt> {
        self.copy_snapshot_with_hook(
            source,
            limits,
            cancel,
            SnapshotProjection::Lossless,
            &mut |_| Ok(()),
        )
    }
    /// Remove protected stored camera metadata before any arbitrary model SQL
    /// can transform it. Sensor values/tags and usable camera/source IDs remain
    /// data. A SQL literal can still echo caller-provided text, as expected.
    /// Stored-row projection alone is not a complete provider-safe boundary:
    /// this on-disk engine still exposes task paths through engine metadata.
    /// Provider runtime queries use Reader::query_model_bounded instead.
    pub fn copy_model_snapshot(
        &mut self,
        source: &Reader,
        limits: SnapshotLimits,
        cancel: &AtomicBool,
    ) -> Result<SnapshotReceipt> {
        self.copy_snapshot_with_hook(
            source,
            limits,
            cancel,
            SnapshotProjection::Model,
            &mut |_| Ok(()),
        )
    }
    fn copy_snapshot_with_hook(
        &mut self,
        source: &Reader,
        limits: SnapshotLimits,
        cancel: &AtomicBool,
        projection: SnapshotProjection,
        after_table: &mut dyn FnMut(&str) -> Result<()>,
    ) -> Result<SnapshotReceipt> {
        limits.validate()?;
        ensure!(
            self.mode != TaskMode::Imported,
            "this task engine already has an approved snapshot"
        );
        ensure!(
            self.mode != TaskMode::Failed,
            "this task snapshot failed; remove its workspace before retrying"
        );
        ensure!(
            self.mode == TaskMode::Empty && self.catalog.is_none(),
            "fresh task catalogs cannot adopt imported snapshots"
        );
        let deadline = Instant::now() + Duration::from_millis(limits.timeout_ms);
        let target = self.connection()?;
        let result = snapshot::copy_into_with_hook(
            source,
            target,
            limits,
            projection,
            cancel,
            deadline,
            after_table,
        );
        if result.is_ok() {
            self.mode = TaskMode::Imported;
            self.model_projection = projection == SnapshotProjection::Model;
        } else {
            self.mode = TaskMode::Failed;
        }
        result
    }
}

#[cfg(all(test, unix, feature = "parquet"))]
mod tests {
    use super::*;
    use crate::dataset::{OpenMode, Slot, task::TaskExtentBudget};
    use std::{path::PathBuf, sync::Arc};

    fn fixture() -> (PathBuf, Arc<Slot>, Reader) {
        let root = std::env::temp_dir().join(format!(
            "capturefab-snapshot-{}-{}",
            std::process::id(),
            crate::dataset::db::random_name().unwrap()
        ));
        crate::ipc::ensure_private_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let slot = Arc::new(Slot::new());
        let status = slot
            .open(&root.join("shared.duckdb"), OpenMode::Create, None)
            .unwrap();
        slot.note_if(
            status.slot_id.as_deref().unwrap(),
            None,
            "before snapshot",
            &crate::context::Context::new(),
        )
        .unwrap();
        let source = slot.reader_if(status.slot_id.as_deref().unwrap()).unwrap();
        source.conn.execute_batch("INSERT INTO captures(wall_ms,camera_id,format,bytes,path,file_name,origin) VALUES(1500,'sim:0','png',1,'/capture.png','capture.png','capture'); INSERT INTO signals(source_id,uri,tag) VALUES('sensor','sim-signal://snapshot','temperature'); INSERT INTO samples(ts_ms,source_id,tag,value_double,quality) VALUES(1400,'sensor','temperature',17.5,'good')").unwrap();
        (root, slot, source)
    }
    fn limits() -> SnapshotLimits {
        SnapshotLimits {
            max_rows: 10_000,
            max_arrow_bytes: 8 * 1024 * 1024,
            max_batch_bytes: 8 * 1024 * 1024,
            timeout_ms: 3000,
        }
    }
    fn target(root: &std::path::Path) -> TaskDataset {
        TaskDataset::create(root, TaskExtentBudget::new(16 * 1024 * 1024).unwrap()).unwrap()
    }
    fn no_published_tables(target: &TaskDataset) {
        assert_eq!(
            target
                .scalar_i64(
                    "SELECT count(*) FROM information_schema.tables WHERE table_schema='main'"
                )
                .unwrap(),
            0,
            "failed copy must roll back all target tables"
        );
        assert!(
            target
                .query_bounded("SELECT 1", 1, 1000, 1024, &AtomicBool::new(false))
                .is_err(),
            "a failed snapshot workspace must be quarantined from user SQL"
        );
    }

    #[test]
    fn one_source_snapshot_remains_consistent_while_writer_commits_and_asof_runs_locally() {
        let (root, slot, source) = fixture();
        let mut target = target(&root);
        let slot_id = source.status.slot_id.clone().unwrap();
        let receipt = target
            .copy_snapshot_with_hook(
                &source,
                limits(),
                &AtomicBool::new(false),
                SnapshotProjection::Model,
                &mut |table| {
                    if table == "dataset_info" {
                        slot.note_if(
                            &slot_id,
                            None,
                            "after snapshot",
                            &crate::context::Context::new(),
                        )?;
                    }
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(receipt.source_slot_id.as_deref(), Some(slot_id.as_str()));
        assert_eq!(receipt.tables["captures"], 1);
        assert_eq!(receipt.tables["samples"], 1);
        assert!(receipt.arrow_bytes > 0);
        assert_eq!(
            target
                .scalar_i64("SELECT count(*) FROM events WHERE text='before snapshot'")
                .unwrap(),
            1
        );
        assert_eq!(
            target
                .scalar_i64("SELECT count(*) FROM events WHERE text='after snapshot'")
                .unwrap(),
            0
        );
        assert_eq!(
            source
                .query(
                    "SELECT count(*) FROM events WHERE text='after snapshot'",
                    1,
                    1000
                )
                .unwrap()["rows"][0][0],
            1
        );
        let value = target
            .query_bounded(
                "SELECT capture_id,value_double,sample_age_ms FROM captures_with_signals",
                10,
                1000,
                4096,
                &AtomicBool::new(false),
            )
            .unwrap();
        assert_eq!(value["rows"][0][1], 17.5);
        assert_eq!(value["rows"][0][2], 100);
        assert!(
            target
                .query_bounded(
                    "SELECT nextval('event_ids')",
                    1,
                    1000,
                    1024,
                    &AtomicBool::new(false)
                )
                .is_err()
        );
        let bounded = target
            .query_bounded(
                "SELECT range,repeat('x',100) FROM range(1000)",
                1000,
                1000,
                512,
                &AtomicBool::new(false),
            )
            .unwrap();
        assert!(serde_json::to_vec(&bounded).unwrap().len() <= 512);
        assert_eq!(bounded["truncation_reason"], "bytes");
        assert!(
            target
                .copy_snapshot(&source, limits(), &AtomicBool::new(false))
                .is_err()
        );
        target.remove().unwrap();
        drop(source);
        slot.close().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn row_transfer_and_batch_caps_abort_without_publishing_partial_tables() {
        let (root, slot, source) = fixture();
        for (limits, reason) in [
            (
                SnapshotLimits {
                    max_rows: 1,
                    ..limits()
                },
                "row limit",
            ),
            (
                SnapshotLimits {
                    max_arrow_bytes: 1,
                    ..limits()
                },
                "transfer byte limit",
            ),
            (
                SnapshotLimits {
                    max_batch_bytes: 1,
                    ..limits()
                },
                "transfer byte limit",
            ),
        ] {
            let mut target = target(&root);
            let error = target
                .copy_snapshot(&source, limits, &AtomicBool::new(false))
                .unwrap_err();
            assert!(format!("{error:#}").contains(reason), "{error:#}");
            no_published_tables(&target);
            target.remove().unwrap();
        }
        drop(source);
        slot.close().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cancellation_and_deadline_rollback_the_entire_target_snapshot() {
        let (root, slot, source) = fixture();
        for deadline in [false, true].into_iter().cycle().take(12) {
            let mut target = target(&root);
            let cancel = AtomicBool::new(false);
            let bounds = if deadline {
                SnapshotLimits {
                    timeout_ms: 10,
                    ..limits()
                }
            } else {
                limits()
            };
            let error = target
                .copy_snapshot_with_hook(
                    &source,
                    bounds,
                    &cancel,
                    SnapshotProjection::Lossless,
                    &mut |table| {
                        if table == "dataset_info" {
                            if deadline {
                                std::thread::sleep(Duration::from_millis(30));
                            } else {
                                cancel.store(true, Ordering::Release);
                            }
                        }
                        Ok(())
                    },
                )
                .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains(if deadline { "deadline" } else { "cancelled" }),
                "{error:#}"
            );
            if !deadline {
                assert!(
                    cancel.load(Ordering::Acquire),
                    "cancellation must happen after BEGIN and the first table scan"
                );
            }
            no_published_tables(&target);
            target.remove().unwrap();
            // Reuse this exact borrowed reader after cancellation/deadline.
            // A plain SELECT alone would not catch a transaction left open;
            // beginning another write transaction must succeed and roll back.
            source
                .conn
                .execute_batch("BEGIN TRANSACTION; UPDATE samples SET value_double=99; ROLLBACK")
                .unwrap();
            assert_eq!(
                source
                    .query("SELECT value_double FROM samples", 1, 1000)
                    .unwrap()["rows"][0][0],
                17.5
            );
        }
        drop(source);
        slot.close().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn closing_the_approved_source_slot_cancels_the_copy_before_adoption() {
        let (root, slot, source) = fixture();
        let mut target = target(&root);
        let (start, receive) = mpsc::channel();
        let closer = {
            let slot = slot.clone();
            std::thread::spawn(move || {
                receive.recv().unwrap();
                slot.close().unwrap();
            })
        };
        let error = target
            .copy_snapshot_with_hook(
                &source,
                limits(),
                &AtomicBool::new(false),
                SnapshotProjection::Lossless,
                &mut |table| {
                    if table == "dataset_info" {
                        start.send(()).unwrap();
                        let deadline = Instant::now() + Duration::from_secs(1);
                        while !source.cancelled.load(Ordering::Acquire) && Instant::now() < deadline
                        {
                            std::thread::yield_now();
                        }
                        ensure!(
                            source.cancelled.load(Ordering::Acquire),
                            "source close did not cancel its registered reader"
                        );
                    }
                    Ok(())
                },
            )
            .unwrap_err();
        assert!(error.to_string().contains("closed"), "{error:#}");
        no_published_tables(&target);
        target.remove().unwrap();
        drop(source);
        closer.join().unwrap();
        assert!(slot.status_cached().is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn allowlisted_names_cannot_substitute_source_views_or_arbitrary_queries() {
        let (root, slot, source) = fixture();
        source.conn.execute_batch("DROP VIEW captures_with_signals; DROP TABLE captures; CREATE VIEW captures AS SELECT * FROM range(1000000000000)").unwrap();
        let mut target = target(&root);
        let error = target
            .copy_snapshot(&source, limits(), &AtomicBool::new(false))
            .unwrap_err();
        assert!(error.to_string().contains("base table"), "{error:#}");
        no_published_tables(&target);
        target.remove().unwrap();
        drop(source);
        slot.close().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn model_projection_removes_stored_metadata_before_arbitrary_sql_can_transform_it() {
        let (root, slot, source) = fixture();
        let home = std::env::var("HOME").unwrap();
        let private_path = format!("{home}/Captures/private-frame.png");
        let object_key = format!("{home}/private-object-key");
        source
            .conn
            .execute(
                "UPDATE captures SET serial=?,source=?,path=?,remote_url=?,object_key=?",
                duckdb::params![
                    "hardware-private-serial",
                    "rtsp://alice:private-password@camera.example/private/video",
                    private_path,
                    "https://u:private-password@objects.example/private/key",
                    object_key
                ],
            )
            .unwrap();
        source.conn.execute("UPDATE signals SET uri=?",["mqtt://alice:private-password@broker.example/private/topic?client_id=private-client"]).unwrap();
        source
            .conn
            .execute("UPDATE samples SET value_text=?", ["opaque-sensor-value"])
            .unwrap();
        let cancel = AtomicBool::new(false);
        let mut lossless = target(&root);
        lossless.copy_snapshot(&source, limits(), &cancel).unwrap();
        assert_eq!(
            lossless
                .scalar_i64("SELECT count(*) FROM captures WHERE serial='hardware-private-serial'")
                .unwrap(),
            1
        );
        assert!(
            lossless
                .query_bounded("SELECT serial FROM captures", 1, 1000, 4096, &cancel)
                .unwrap_err()
                .to_string()
                .contains("projected")
        );
        lossless.remove().unwrap();
        let mut model = target(&root);
        assert_eq!(
            model
                .copy_model_snapshot(&source, limits(), &cancel)
                .unwrap()
                .projection,
            SnapshotProjection::Model
        );
        let captures=model.query_bounded("SELECT serial,replace(source,'://','_'),replace(path,'/','|'),replace(remote_url,'://','_'),replace(object_key,'/','|'),camera_id FROM captures",10,1000,8192,&cancel).unwrap();
        let sources = model
            .query_bounded(
                "SELECT replace(uri,'/','|'),source_id,tag FROM signals",
                10,
                1000,
                8192,
                &cancel,
            )
            .unwrap();
        let combined = format!("{captures}{sources}");
        for private in [
            "hardware-private-serial",
            "private-password",
            "private|video",
            "private|key",
            "private|topic",
            "private-client",
            home.as_str(),
            home.replace('/', "|").as_str(),
        ] {
            assert!(
                !combined.contains(private),
                "protected stored metadata escaped projection: {private}: {combined}"
            );
        }
        assert!(captures["rows"][0][0].is_null());
        assert_eq!(captures["rows"][0][5], "sim:0");
        assert_eq!(sources["rows"][0][1], "sensor");
        assert_eq!(sources["rows"][0][2], "temperature");
        assert_eq!(
            model
                .query_bounded("SELECT value_text FROM samples", 1, 1000, 4096, &cancel)
                .unwrap()["rows"][0][0],
            "opaque-sensor-value"
        );
        assert_eq!(
            model
                .query_bounded(
                    "SELECT 'literal-provided-by-caller'",
                    1,
                    1000,
                    4096,
                    &cancel
                )
                .unwrap()["rows"][0][0],
            "literal-provided-by-caller"
        );
        model.remove().unwrap();
        drop(source);
        slot.close().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn snapshot_copy_cannot_extend_target_files_past_its_shared_extent_lease() {
        let (root, slot, source) = fixture();
        source.conn.execute_batch("INSERT INTO events(kind,wall_ms,text) SELECT 'payload',i,repeat(md5(i::VARCHAR),64) FROM range(3000) t(i)").unwrap();
        let budget = TaskExtentBudget::new(1024 * 1024).unwrap();
        let mut target = TaskDataset::create(&root, budget.clone()).unwrap();
        let error = target
            .copy_snapshot(&source, limits(), &AtomicBool::new(false))
            .unwrap_err();
        assert!(format!("{error:#}").contains("extent limit"), "{error:#}");
        no_published_tables(&target);
        let usage = budget.usage().unwrap();
        assert!(usage.denied > 0 && usage.peak <= usage.limit);
        target.remove().unwrap();
        assert_eq!(budget.usage().unwrap().used, 0);
        drop(source);
        slot.close().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn model_query_cancellation_interrupts_only_the_isolated_engine() {
        let (root, slot, source) = fixture();
        let mut target = target(&root);
        target
            .copy_model_snapshot(&source, limits(), &AtomicBool::new(false))
            .unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let setter = {
            let cancel = cancel.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(20));
                cancel.store(true, Ordering::Release);
            })
        };
        let started = Instant::now();
        let error = target
            .query_bounded(
                "SELECT sum(sin(i)) FROM range(1000000000000) t(i)",
                1,
                1000,
                1024,
                &cancel,
            )
            .unwrap_err();
        setter.join().unwrap();
        assert!(error.to_string().contains("cancelled"), "{error:#}");
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(
            source
                .query("SELECT count(*) FROM captures", 1, 1000)
                .unwrap()["rows"][0][0],
            1
        );
        target.remove().unwrap();
        drop(source);
        slot.close().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
}
