//! Canonical typed catalog writes for one owning task writer. There is no
//! Connection/clone escape, path-based adoption or provider SQL in this API.
use super::{ExtentUsage, TaskDataset, TaskMode};
use crate::{
    context::Context,
    dataset::{DatasetStatus, write},
};
use anyhow::{Context as _, Result, ensure};
use serde::Serialize;

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
}
impl Catalog {
    pub(super) fn record_error(&mut self, error: &anyhow::Error) {
        self.status.last_error = Some(format!("{error:#}"));
    }
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
            ensure!(!id.is_empty(), "event source identity cannot be empty");
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
    use crate::dataset::{OpenMode, Slot, task::TaskExtentBudget};
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
            assert!(
                dataset
                    .record_event("note", None, None, "note", &Context::new())
                    .is_err()
            );
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
        dataset
            .record_event(
                "note",
                Some("sim:0"),
                None,
                "Captured frame",
                &Context::new(),
            )
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
        assert_eq!(readonly.status().events, 1);
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
        assert_eq!(retained().unwrap()["phase"], "retained");
        dataset.close().unwrap();
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
    fn retained_constructor_closes_without_refund_and_explicit_remove_releases_its_charge() {
        let parent = parent("retained-constructor");
        let cap = 8 * 1024 * 1024;
        let mut data =
            TaskDataset::create_retained(&parent, &crate::storage::StoragePolicy::default(), cap)
                .unwrap();
        data.initialize_catalog("Camera captures").unwrap();
        let path = data.path().to_owned();
        data.close().unwrap();
        let inventory = crate::storage::quota_status().unwrap();
        let entry = inventory["retained_namespaces"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["path"].as_str() == path.to_str())
            .unwrap();
        assert_eq!(entry["allocated_bytes"], cap);
        assert_eq!(entry["phase"], "retained");
        let id = entry["id"].clone();
        data.remove().unwrap();
        assert!(!path.exists());
        assert!(
            crate::storage::quota_status().unwrap()["retained_namespaces"]
                .as_array()
                .unwrap()
                .iter()
                .all(|entry| entry["id"] != id)
        );
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
