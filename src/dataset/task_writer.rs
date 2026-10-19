//! Canonical typed catalog writes for one owning task writer. There is no
//! Connection/clone escape, path-based adoption or provider SQL in this API.
use super::{ExtentUsage, TaskDataset, TaskMode};
use crate::{
    context::Context,
    dataset::{DatasetStatus, write},
    signals::{Sample, SignalInfo, TagInfo},
};
use anyhow::{Context as _, Result, ensure};
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TaskCatalogIdentity {
    pub dataset_id: String,
    pub slot_id: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct TaskCatalogStatus {
    pub identity: TaskCatalogIdentity,
    pub dataset: DatasetStatus,
    /// The shared task lease includes every engine/retained namespace using it.
    /// This is combined logical EOF accounting, not physical blocks or RSS.
    pub extents: ExtentUsage,
    pub quarantined: bool,
    /// True only after definite native engine shutdown, never merely because
    /// the client connection was dropped during an uncertain close.
    pub closed: bool,
}

pub(super) struct Catalog {
    identity: TaskCatalogIdentity,
    status: DatasetStatus,
    sources: BTreeMap<String, (String, String)>,
}
impl Catalog {
    pub(super) fn record_error(&mut self, error: &anyhow::Error) {
        self.status.last_error = Some(format!("{error:#}"));
    }
}

fn source_id(id: &str) -> Result<()> {
    crate::signals::validate_id(id)?;
    ensure!(
        id != "all",
        "a task catalog requires one exact source identity"
    );
    Ok(())
}

impl TaskDataset {
    /// Initialize only a fresh empty engine. Imported IDs and sequences are
    /// never silently adopted by an acquisition writer.
    pub fn initialize_catalog(&mut self, name: &str) -> Result<TaskCatalogIdentity> {
        ensure!(
            self.mode == TaskMode::Empty && self.catalog.is_none(),
            "task catalog initialization requires a fresh empty engine"
        );
        ensure!(
            !name.is_empty() && name.len() <= 256 && !name.chars().any(char::is_control),
            "task catalog name must be 1..256 bytes without control characters"
        );
        self.connection()?;
        let identity = TaskCatalogIdentity {
            dataset_id: crate::dataset::db::random_name()?,
            slot_id: crate::dataset::new_slot_id()?,
        };
        let created = crate::dataset::db::now_ms();
        let status = DatasetStatus {
            path: self.database_path().display().to_string(),
            dataset_id: identity.dataset_id.clone(),
            slot_id: Some(identity.slot_id.clone()),
            name: name.into(),
            created_ms: created,
            max_bytes: self.usage()?.limit,
            ..Default::default()
        };
        self.catalog = Some(Catalog {
            identity: identity.clone(),
            status,
            sources: BTreeMap::new(),
        });
        let result = (|| -> Result<()> {
            let conn = self.connection()?;
            let existing: u64 = conn.query_row(
                "SELECT sum(n)::UBIGINT FROM (
                    SELECT count(*) n FROM system.main.duckdb_tables() WHERE database_name IN (system.main.current_database(),'temp') AND NOT internal
                    UNION ALL SELECT count(*) FROM system.main.duckdb_views() WHERE database_name IN (system.main.current_database(),'temp') AND NOT internal
                    UNION ALL SELECT count(*) FROM system.main.duckdb_sequences() WHERE database_name IN (system.main.current_database(),'temp')
                    UNION ALL SELECT count(*) FROM system.main.duckdb_functions() WHERE database_name IN (system.main.current_database(),'temp') AND NOT internal
                )", [], |row| row.get(0),
            )?;
            ensure!(
                existing == 0,
                "task catalog initialization cannot adopt existing catalog objects"
            );
            let max = self
                .catalog
                .as_ref()
                .expect("catalog prepared")
                .status
                .max_bytes;
            conn.execute_batch("BEGIN TRANSACTION")?;
            let result = write::initialize(conn, &identity.dataset_id, name, created, max);
            match result {
                Ok(()) => conn.execute_batch("COMMIT").map_err(Into::into),
                Err(error) => {
                    let _ = conn.execute_batch("ROLLBACK");
                    Err(error)
                }
            }
        })();
        self.finish_catalog_operation(result)?;
        self.mode = TaskMode::Fresh;
        self.checkpoint_catalog()?;
        Ok(identity)
    }

    pub fn catalog_identity(&self) -> Option<&TaskCatalogIdentity> {
        self.catalog.as_ref().map(|catalog| &catalog.identity)
    }

    /// A cached catalog receipt remains available after quarantine or close.
    /// Extent accounting remains authoritative for reservations retained by an
    /// uncertain write, checkpoint, unlink or final close.
    pub fn catalog_status(&self) -> Result<TaskCatalogStatus> {
        let catalog = self
            .catalog
            .as_ref()
            .context("this task engine has no fresh capture catalog")?;
        let mut dataset = catalog.status.clone();
        let size = |path: &std::path::Path| {
            std::fs::metadata(path)
                .map(|metadata| metadata.len())
                .unwrap_or(0)
        };
        dataset.bytes = size(&self.database_path())
            + size(&crate::dataset::db::wal_path(&self.database_path()));
        Ok(TaskCatalogStatus {
            identity: catalog.identity.clone(),
            dataset,
            extents: self.usage()?,
            quarantined: self.mode == TaskMode::Failed,
            closed: self.conn.is_none() && self.database.is_null() && !self.close_uncertain,
        })
    }

    fn writable_catalog(&self) -> Result<()> {
        ensure!(
            self.mode != TaskMode::Failed,
            "task catalog is quarantined after a storage failure; retain it for recovery"
        );
        ensure!(
            self.mode == TaskMode::Fresh && self.catalog.is_some(),
            "this task engine has no fresh capture catalog"
        );
        self.connection()?;
        Ok(())
    }

    fn finish_catalog_operation<T>(&mut self, result: Result<T>) -> Result<T> {
        if let Err(error) = &result {
            self.mode = TaskMode::Failed;
            if let Some(catalog) = &mut self.catalog {
                catalog.record_error(error);
            }
        }
        result
    }

    fn catalog_transaction(
        &mut self,
        operation: impl FnOnce(&duckdb::Connection) -> Result<()>,
    ) -> Result<()> {
        let result = (|| -> Result<()> {
            let conn = self.connection()?;
            conn.execute_batch("BEGIN TRANSACTION")?;
            match operation(conn) {
                Ok(()) => conn.execute_batch("COMMIT").map_err(Into::into),
                Err(error) => {
                    let _ = conn.execute_batch("ROLLBACK");
                    Err(error)
                }
            }
        })();
        self.finish_catalog_operation(result)
    }

    pub fn register(&mut self, source: &SignalInfo, tags: &[TagInfo]) -> Result<()> {
        self.writable_catalog()?;
        source_id(&source.id)?;
        ensure!(
            tags.len() <= crate::signals::MAX_TAGS,
            "too many task source tags"
        );
        let protocol = write::protocol(source)?;
        let catalog = self.catalog.as_ref().expect("checked catalog");
        ensure!(
            catalog
                .sources
                .get(&source.id)
                .is_none_or(|known| known == &(source.uri.clone(), protocol.clone())),
            "task catalog source identity changed; do not replace its registered endpoint"
        );
        self.catalog_transaction(|conn| write::register(conn, source, tags, &protocol))?;
        let count = self
            .connection()?
            .query_row("SELECT count(*) FROM signals", [], |row| {
                row.get::<_, u64>(0)
            })
            .map_err(Into::into);
        let count = self.finish_catalog_operation(count)?;
        let catalog = self.catalog.as_mut().expect("checked catalog");
        catalog
            .sources
            .insert(source.id.clone(), (source.uri.clone(), protocol));
        catalog.status.signals = count;
        Ok(())
    }

    pub fn append_samples(&mut self, id: &str, batch: &[Sample]) -> Result<usize> {
        self.writable_catalog()?;
        source_id(id)?;
        ensure!(
            batch.len() <= crate::dataset::db::MAX_SAMPLES_PER_CALL,
            "at most {} samples per task write",
            crate::dataset::db::MAX_SAMPLES_PER_CALL
        );
        let (uri, protocol) = self
            .catalog
            .as_ref()
            .expect("checked catalog")
            .sources
            .get(id)
            .cloned()
            .context("task samples require an explicitly registered source")?;
        if batch.is_empty() {
            return Ok(0);
        }
        let range = write::sample_ranges(batch);
        self.catalog_transaction(|conn| {
            write::append_samples(conn, id, &uri, &protocol, batch, &range)
        })?;
        let count = self
            .connection()?
            .query_row("SELECT count(*) FROM signals", [], |row| {
                row.get::<_, u64>(0)
            })
            .map_err(Into::into);
        let count = self.finish_catalog_operation(count)?;
        let catalog = self.catalog.as_mut().expect("checked catalog");
        catalog.status.signals = count;
        catalog.status.samples += batch.len() as u64;
        if let (Some(first), Some(last)) = (
            range.values().map(|range| range.0).min(),
            range.values().map(|range| range.1).max(),
        ) {
            catalog.status.first_ms = Some(
                catalog
                    .status
                    .first_ms
                    .map_or(first, |known| known.min(first)),
            );
            catalog.status.last_ms =
                Some(catalog.status.last_ms.map_or(last, |known| known.max(last)));
        }
        Ok(batch.len())
    }

    pub fn record_event(
        &mut self,
        kind: &str,
        camera_id: Option<&str>,
        source: Option<&str>,
        text: &str,
        context: &Context,
    ) -> Result<u64> {
        self.writable_catalog()?;
        ensure!(
            !kind.is_empty() && kind.len() <= 64 && !kind.chars().any(char::is_control),
            "task event kind must be 1..64 bytes without control characters"
        );
        ensure!(
            !text.is_empty() && text.len() <= crate::dataset::MAX_NOTE_BYTES,
            "task event text must be 1..4096 bytes"
        );
        crate::context::validate(context)?;
        if let Some(id) = source {
            source_id(id)?;
            ensure!(
                self.catalog
                    .as_ref()
                    .expect("checked catalog")
                    .sources
                    .contains_key(id),
                "task events require an explicitly registered source"
            );
        }
        let result = write::record_event(
            self.connection()?,
            crate::dataset::db::now_ms(),
            kind,
            camera_id,
            source,
            text,
            context,
        )
        .map_err(Into::into);
        let id = self.finish_catalog_operation(result)?;
        self.catalog
            .as_mut()
            .expect("checked catalog")
            .status
            .events += 1;
        Ok(id)
    }

    pub fn checkpoint_catalog(&mut self) -> Result<()> {
        self.writable_catalog()?;
        let result = self
            .connection()?
            .execute_batch("CHECKPOINT")
            .map_err(Into::into);
        self.finish_catalog_operation(result)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::{
        dataset::{OpenMode, Slot, task::TaskExtentBudget},
        signals::{Protocol, Quality, Value},
    };
    use sha2::{Digest, Sha256};
    use std::path::PathBuf;
    #[cfg(feature = "parquet")]
    use std::sync::atomic::AtomicBool;

    fn parent(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "capturefab-task-catalog-{label}-{}-{}",
            std::process::id(),
            crate::dataset::db::random_name().unwrap()
        ));
        crate::ipc::ensure_private_dir(&path).unwrap();
        path.canonicalize().unwrap()
    }
    fn source() -> SignalInfo {
        let uri = "sim-signal://task-catalog";
        SignalInfo {
            id: crate::signals::source_id(uri),
            protocol: Protocol::SimSignal,
            uri: uri.into(),
            interval_ms: 10,
        }
    }
    fn sample(ts_ms: u64, tag: &str, value: Value) -> Sample {
        Sample {
            ts_ms,
            source_ts_ms: Some(ts_ms - 1),
            tag: tag.into(),
            value,
            quality: Quality::Good,
        }
    }
    fn payload(offset: usize, rows: usize) -> Vec<Sample> {
        (offset..offset + rows)
            .map(|row| {
                let text: String = (0..16)
                    .map(|block| {
                        format!("{:x}", Sha256::digest(format!("{row}:{block}").as_bytes()))
                    })
                    .collect();
                sample(row as u64 + 1, "payload", Value::Text(text))
            })
            .collect()
    }
    fn scalar(dataset: &TaskDataset, sql: &str) -> u64 {
        dataset
            .connection()
            .unwrap()
            .query_row(sql, [], |row| row.get(0))
            .unwrap()
    }

    #[test]
    fn typed_catalog_writes_canonical_signals_events_and_stable_fresh_identity() {
        let parent = parent("typed");
        let budget = TaskExtentBudget::new(16 * 1024 * 1024).unwrap();
        let mut dataset = TaskDataset::create(&parent, budget.clone()).unwrap();
        let identity = dataset.initialize_catalog("typed task").unwrap();
        assert_ne!(identity.dataset_id, identity.slot_id);
        assert_eq!(identity.slot_id.len(), 32);
        let info = source();
        dataset
            .register(
                &info,
                &[TagInfo {
                    tag: "temperature".into(),
                    unit: Some("C".into()),
                    kind: Some("number".into()),
                }],
            )
            .unwrap();
        let mut samples = vec![
            sample(3000, "temperature", Value::Number(42.25)),
            sample(1000, "ready", Value::Bool(true)),
            sample(2000, "state", Value::Text("running".into())),
        ];
        samples[2].quality = Quality::Uncertain;
        assert_eq!(dataset.append_samples(&info.id, &samples).unwrap(), 3);
        let context = Context::from([("station".into(), "A1".into())]);
        assert_eq!(
            dataset
                .record_event("capture", Some("sim:0"), Some(&info.id), "saved", &context)
                .unwrap(),
            1
        );
        assert_eq!(
            dataset
                .record_event("note", None, None, "reviewed", &Context::new())
                .unwrap(),
            2
        );
        dataset.checkpoint_catalog().unwrap();
        let status = dataset.catalog_status().unwrap();
        assert_eq!(status.identity, identity);
        assert_eq!(status.dataset.dataset_id, identity.dataset_id);
        assert_eq!(status.dataset.slot_id.as_ref(), Some(&identity.slot_id));
        assert_eq!(status.dataset.samples, 3);
        assert_eq!(status.dataset.signals, 3);
        assert_eq!(status.dataset.events, 2);
        assert_eq!(
            (status.dataset.first_ms, status.dataset.last_ms),
            (Some(1000), Some(3000))
        );
        assert!(!status.quarantined && !status.closed);
        assert!(status.dataset.bytes > 0);
        assert!(status.extents.peak <= status.extents.limit);
        assert_eq!(
            scalar(
                &dataset,
                "SELECT count(*) FROM samples WHERE tag='temperature' AND value_double=42.25 AND value_text IS NULL"
            ),
            1
        );
        assert_eq!(
            scalar(
                &dataset,
                "SELECT count(*) FROM samples WHERE tag='ready' AND value_double=1 AND value_text='true'"
            ),
            1
        );
        assert_eq!(
            scalar(
                &dataset,
                "SELECT count(*) FROM samples WHERE tag='state' AND value_text='running' AND value_double IS NULL AND quality='uncertain'"
            ),
            1
        );
        assert_eq!(
            scalar(
                &dataset,
                "SELECT count(*) FROM events WHERE id=1 AND camera_id='sim:0' AND context='{\"station\":\"A1\"}'"
            ),
            1
        );
        assert_eq!(
            scalar(
                &dataset,
                "SELECT count(*) FROM signals WHERE tag='temperature' AND unit='C' AND first_ms=3000 AND last_ms=3000"
            ),
            1
        );
        let pristine = duckdb::Connection::open_in_memory().unwrap();
        pristine.execute_batch(crate::dataset::db::DDL).unwrap();
        let table_sql = |conn: &duckdb::Connection| -> Vec<String> {
            conn.prepare("SELECT sql FROM system.main.duckdb_tables() WHERE database_name=system.main.current_database() AND NOT internal ORDER BY table_name")
                .unwrap()
                .query_map([], |row| row.get(0))
                .unwrap()
                .collect::<duckdb::Result<_>>()
                .unwrap()
        };
        assert_eq!(
            table_sql(dataset.connection().unwrap()),
            table_sql(&pristine)
        );
        assert!(dataset.execute_batch("DELETE FROM samples").is_err());
        assert!(dataset.scalar_i64("SELECT 1").is_err());
        let path = dataset.path().to_owned();
        dataset.close().unwrap();
        let status = dataset.catalog_status().unwrap();
        assert!(status.closed);
        assert_eq!(status.identity, identity);
        assert!(dataset.append_samples(&info.id, &samples).is_err());
        dataset.remove().unwrap();
        assert!(!path.exists());
        assert_eq!(budget.usage().unwrap().used, 0);
        let mut other = TaskDataset::create(&parent, budget.clone()).unwrap();
        assert_ne!(other.initialize_catalog("next task").unwrap(), identity);
        other.remove().unwrap();
        std::fs::remove_dir(parent).unwrap();
    }

    #[test]
    fn typed_catalog_refuses_unregistered_or_replaced_source_without_quarantine() {
        let parent = parent("sources");
        let budget = TaskExtentBudget::new(8 * 1024 * 1024).unwrap();
        let mut dataset = TaskDataset::create(&parent, budget).unwrap();
        dataset.initialize_catalog("sources").unwrap();
        let info = source();
        let samples = [sample(1, "temperature", Value::Number(3.0))];
        assert!(dataset.append_samples(&info.id, &samples).is_err());
        assert!(
            dataset
                .record_event(
                    "note",
                    None,
                    Some(&info.id),
                    "unregistered",
                    &Context::new()
                )
                .is_err()
        );
        dataset.register(&info, &[]).unwrap();
        let mut replacement = info.clone();
        replacement.uri = "sim-signal://another-endpoint".into();
        assert!(dataset.register(&replacement, &[]).is_err());
        assert!(dataset.append_samples("all", &samples).is_err());
        assert_eq!(dataset.append_samples(&info.id, &samples).unwrap(), 1);
        assert_eq!(
            scalar(
                &dataset,
                "SELECT count(*) FROM signals WHERE uri='sim-signal://task-catalog'"
            ),
            1
        );
        assert!(!dataset.catalog_status().unwrap().quarantined);
        dataset.remove().unwrap();
        std::fs::remove_dir(parent).unwrap();
    }

    #[test]
    fn typed_catalog_initialization_never_adopts_existing_tables_or_catalog_code() {
        let parent = parent("foreign");
        let budget = TaskExtentBudget::new(8 * 1024 * 1024).unwrap();
        for setup in [
            "CREATE TABLE foreign_data AS SELECT 42 AS value",
            "CREATE VIEW foreign_data AS SELECT 42 AS value",
            "CREATE MACRO nextval(value) AS 42",
            "CREATE TEMP MACRO event_ids() AS 42",
            "CREATE SEQUENCE foreign_ids",
        ] {
            let mut dataset = TaskDataset::create(&parent, budget.clone()).unwrap();
            dataset.execute_batch(setup).unwrap();
            let error = dataset.initialize_catalog("foreign").unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("cannot adopt existing catalog objects"),
                "{setup}: {error:#}"
            );
            assert!(dataset.catalog_status().unwrap().quarantined);
            assert!(dataset.initialize_catalog("retry").is_err());
            assert!(dataset.register(&source(), &[]).is_err());
            dataset.remove().unwrap();
        }
        std::fs::remove_dir(parent).unwrap();
    }

    #[cfg(feature = "parquet")]
    #[test]
    fn typed_catalog_and_imported_snapshot_cannot_be_mixed() {
        let parent = parent("snapshot");
        let slot = Slot::new();
        slot.open(&parent.join("shared.duckdb"), OpenMode::Create, None)
            .unwrap();
        let reader = slot.reader().unwrap();
        let budget = TaskExtentBudget::new(16 * 1024 * 1024).unwrap();
        let mut fresh = TaskDataset::create(&parent, budget.clone()).unwrap();
        fresh.initialize_catalog("fresh").unwrap();
        let limits = crate::dataset::task::SnapshotLimits {
            max_rows: 100,
            max_arrow_bytes: 1024 * 1024,
            max_batch_bytes: 1024 * 1024,
            timeout_ms: 3000,
        };
        let cancel = AtomicBool::new(false);
        assert!(fresh.copy_snapshot(&reader, limits, &cancel).is_err());
        assert!(!fresh.catalog_status().unwrap().quarantined);
        let mut imported = TaskDataset::create(&parent, budget).unwrap();
        imported.copy_snapshot(&reader, limits, &cancel).unwrap();
        assert!(imported.initialize_catalog("adopt").is_err());
        assert!(imported.catalog_identity().is_none());
        imported.remove().unwrap();
        fresh.remove().unwrap();
        drop(reader);
        slot.close().unwrap();
        std::fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn typed_append_quota_failure_quarantines_and_retains_extents_without_shared_writes() {
        let parent = parent("append-quota");
        let shared = Slot::new();
        shared
            .open(&parent.join("shared.duckdb"), OpenMode::Create, None)
            .unwrap();
        let before = shared.status_cached().unwrap();
        let budget = TaskExtentBudget::new(4 * 1024 * 1024).unwrap();
        let mut dataset = TaskDataset::create(&parent, budget.clone()).unwrap();
        dataset.initialize_catalog("bounded append").unwrap();
        let info = source();
        dataset.register(&info, &[]).unwrap();
        let mut refused = false;
        for batch in 0..32 {
            let result = dataset.append_samples(&info.id, &payload(batch * 1000, 1000));
            if let Err(error) = result {
                assert!(format!("{error:#}").contains("extent limit"), "{error:#}");
                refused = true;
                break;
            }
        }
        assert!(refused, "typed writes must exhaust the 4 MiB extent cap");
        let status = dataset.catalog_status().unwrap();
        assert!(status.quarantined);
        assert!(status.dataset.last_error.is_some());
        assert!(status.extents.used > 0 && status.extents.denied > 0);
        assert!(status.extents.peak <= status.extents.limit);
        assert!(dataset.append_samples(&info.id, &[]).is_err());
        assert!(dataset.register(&info, &[]).is_err());
        assert!(
            dataset
                .record_event("note", None, None, "after failure", &Context::new())
                .is_err()
        );
        assert!(dataset.checkpoint_catalog().is_err());
        let after = shared.status_cached().unwrap();
        assert_eq!(before.dataset_id, after.dataset_id);
        assert_eq!(before.slot_id, after.slot_id);
        assert_eq!(
            (after.samples, after.signals, after.events),
            (before.samples, before.signals, before.events)
        );
        dataset.remove().unwrap();
        assert_eq!(budget.usage().unwrap().used, 0);
        shared.close().unwrap();
        std::fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn typed_checkpoint_quota_failure_quarantines_without_releasing_retained_charge() {
        let parent = parent("checkpoint-quota");
        let budget = TaskExtentBudget::new(8 * 1024 * 1024).unwrap();
        let mut dataset = TaskDataset::create(&parent, budget.clone()).unwrap();
        dataset.initialize_catalog("bounded checkpoint").unwrap();
        let info = source();
        dataset.register(&info, &[]).unwrap();
        let usage = budget.usage().unwrap();
        let remaining = usage.limit - usage.used;
        let rows = ((remaining * 9 / 10) / (crate::signals::MAX_TEXT as u64 + 128)) as usize;
        assert!((100..=crate::dataset::db::MAX_SAMPLES_PER_CALL).contains(&rows));
        dataset.append_samples(&info.id, &payload(0, rows)).unwrap();
        let error = dataset.checkpoint_catalog().unwrap_err();
        assert!(format!("{error:#}").contains("extent limit"), "{error:#}");
        let status = dataset.catalog_status().unwrap();
        assert!(status.quarantined && status.extents.denied > 0);
        assert!(status.extents.used > 0 && status.extents.peak <= status.extents.limit);
        assert!(dataset.append_samples(&info.id, &payload(rows, 1)).is_err());
        dataset.close().unwrap();
        assert!(budget.usage().unwrap().used > 0);
        dataset.remove().unwrap();
        assert_eq!(budget.usage().unwrap().used, 0);
        std::fs::remove_dir(parent).unwrap();
    }

    #[test]
    fn reserved_catalog_uses_exact_anchor_and_retains_global_cap_until_removed_receipt() {
        let parent = parent("reserved");
        let cap = 8 * 1024 * 1024;
        let lease = crate::storage::reserve_task_namespace(
            &parent,
            &crate::storage::StoragePolicy::default(),
            cap,
        )
        .unwrap();
        let path = lease.anchor().path().to_owned();
        let id = lease.id().to_owned();
        let budget = TaskExtentBudget::new(cap).unwrap();
        let mut dataset = TaskDataset::create_reserved(lease.clone(), budget.clone()).unwrap();
        assert_eq!(dataset.path(), path);
        let nested_budget = TaskExtentBudget::new(cap).unwrap();
        assert!(TaskDataset::create(dataset.path(), nested_budget.clone()).is_err());
        assert_eq!(nested_budget.usage().unwrap().used, 0);
        dataset.initialize_catalog("reserved task").unwrap();
        let info = source();
        dataset.register(&info, &[]).unwrap();
        dataset
            .append_samples(&info.id, &[sample(1, "value", Value::Number(7.0))])
            .unwrap();
        dataset.checkpoint_catalog().unwrap();
        let error =
            crate::dataset::db::Dataset::open(&dataset.database_path(), OpenMode::Open, None)
                .err()
                .unwrap();
        assert!(error.to_string().contains("reserved task namespace"));
        let new_path = dataset.path().join("bypass.duckdb");
        assert!(crate::dataset::db::Dataset::open(&new_path, OpenMode::Create, None).is_err());
        assert!(!new_path.exists());
        dataset.close().unwrap();
        let readonly =
            crate::dataset::db::Dataset::open_read_only(&dataset.database_path()).unwrap();
        assert_eq!(readonly.status().samples, 1);
        drop(readonly);
        let retained = || {
            crate::storage::quota_status().unwrap()["retained_namespaces"]
                .as_array()
                .unwrap()
                .iter()
                .find(|entry| entry["id"] == id)
                .cloned()
        };
        assert_eq!(retained().unwrap()["allocated_bytes"], cap);
        let receipt = dataset.remove_reserved().unwrap();
        assert!(!path.exists());
        assert_eq!(budget.usage().unwrap().used, 0);
        assert!(
            retained().is_some(),
            "native removal alone cannot refund global storage"
        );
        lease.confirm_removed(&receipt).unwrap();
        assert!(retained().is_none());
        assert!(dataset.catalog_status().unwrap().closed);
        drop(dataset);
        std::fs::remove_dir(parent).unwrap();
    }

    #[test]
    fn reserved_catalog_requires_fresh_exclusive_matching_budget_before_engine_admission() {
        let parent = parent("reserved-admission");
        let cap = 8 * 1024 * 1024;
        let lease = crate::storage::reserve_task_namespace(
            &parent,
            &crate::storage::StoragePolicy::default(),
            cap,
        )
        .unwrap();
        assert!(
            TaskDataset::create_reserved(lease.clone(), TaskExtentBudget::new(cap / 2).unwrap(),)
                .is_err()
        );
        assert!(lease.anchor().path().read_dir().unwrap().next().is_none());
        let used = TaskExtentBudget::new(cap).unwrap();
        let old = TaskDataset::create(&parent, used.clone()).unwrap();
        old.remove().unwrap();
        assert_eq!(used.usage().unwrap().used, 0);
        assert!(TaskDataset::create_reserved(lease.clone(), used).is_err());
        let budget = TaskExtentBudget::new(cap).unwrap();
        let mut dataset = TaskDataset::create_reserved(lease.clone(), budget.clone()).unwrap();
        let before = std::fs::read_dir(&parent).unwrap().count();
        assert!(TaskDataset::create(&parent, budget).is_err());
        assert_eq!(std::fs::read_dir(&parent).unwrap().count(), before);
        assert!(
            TaskDataset::create_reserved(lease.clone(), TaskExtentBudget::new(cap).unwrap())
                .is_err()
        );
        let receipt = dataset.remove_reserved().unwrap();
        lease.confirm_removed(&receipt).unwrap();
        drop(dataset);
        std::fs::remove_dir(parent).unwrap();
    }

    #[test]
    fn reserved_engine_open_failure_preserves_cause_and_refunds_only_after_native_cleanup() {
        let parent = parent("reserved-open-failure");
        let cap = 1024;
        let lease = crate::storage::reserve_task_namespace(
            &parent,
            &crate::storage::StoragePolicy::default(),
            cap,
        )
        .unwrap();
        let path = lease.anchor().path().to_owned();
        let budget = TaskExtentBudget::new(cap).unwrap();
        let error = TaskDataset::create_reserved(lease.clone(), budget.clone())
            .err()
            .unwrap();
        let diagnostic = format!("{error:#}");
        assert!(diagnostic.contains("extent limit"), "{diagnostic}");
        assert!(diagnostic.contains("was removed"), "{diagnostic}");
        assert!(!path.exists());
        let usage = budget.usage().unwrap();
        assert_eq!(usage.used, 0);
        assert_eq!(usage.uncertain_files, 0);
        assert!(usage.denied > 0 && usage.peak <= cap);
        assert!(
            crate::storage::quota_status().unwrap()["retained_namespaces"]
                .as_array()
                .unwrap()
                .iter()
                .all(|entry| entry["id"] != lease.id())
        );
        std::fs::remove_dir(parent).unwrap();
    }

    #[test]
    fn closed_receipt_requires_definite_native_shutdown_and_uncertainty_retains_the_charge() {
        let parent = parent("uncertain-close-receipt");
        let cap = 8 * 1024 * 1024;
        let lease = crate::storage::reserve_task_namespace(
            &parent,
            &crate::storage::StoragePolicy::default(),
            cap,
        )
        .unwrap();
        let budget = TaskExtentBudget::new(cap).unwrap();
        let mut dataset = TaskDataset::create_reserved(lease.clone(), budget.clone()).unwrap();
        dataset.initialize_catalog("shutdown receipt").unwrap();
        dataset.close().unwrap();
        assert!(dataset.catalog_status().unwrap().closed);
        let retained = budget.usage().unwrap().used;
        assert!(retained > 0);

        // The engine really closed above. Inject only the receipt's failure
        // state to verify its contract, without claiming a native close fault.
        dataset.close_uncertain = true;
        dataset.mode = TaskMode::Failed;
        dataset
            .catalog
            .as_mut()
            .unwrap()
            .record_error(&anyhow::anyhow!("injected uncertain shutdown receipt"));
        let status = dataset.catalog_status().unwrap();
        assert!(!status.closed && status.quarantined);
        let error = dataset.remove_reserved().err().unwrap();
        assert!(error.to_string().contains("close is uncertain"));
        assert!(dataset.path().exists());
        assert_eq!(budget.usage().unwrap().used, retained);
        assert_eq!(
            crate::storage::quota_status().unwrap()["retained_namespaces"]
                .as_array()
                .unwrap()
                .iter()
                .find(|entry| entry["id"] == lease.id())
                .unwrap()["allocated_bytes"],
            cap
        );

        // Restore the injected flag solely to clean this actually closed
        // engine. Production never clears an uncertain-close fence.
        dataset.close_uncertain = false;
        let receipt = dataset.remove_reserved().unwrap();
        lease.confirm_removed(&receipt).unwrap();
        drop(dataset);
        std::fs::remove_dir(parent).unwrap();
    }
}
