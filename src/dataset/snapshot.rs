//! Fixed base-table scans into the isolated task engine. Shared-engine joins,
//! sorts, ASOF evaluation and arbitrary user SQL never enter this path.
//!
//! The source still owns its buffer manager. A simple scan avoids explicit
//! spill operators, but shared-engine spill indirectly caused by memory pressure
//! remains outside this task filesystem's logical-extent guarantee.
use crate::dataset::Reader;
use anyhow::{Context as _, Result, ensure};
use duckdb::{
    Connection,
    arrow::{
        array::{Array, ArrayRef, LargeStringArray, StringArray, StringViewArray, new_null_array},
        datatypes::DataType,
        record_batch::RecordBatch,
    },
};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

const BASE_TABLES: [&str; 6] = [
    "dataset_info",
    "captures",
    "recordings",
    "signals",
    "samples",
    "events",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotProjection {
    Lossless,
    Model,
}

fn project_model_metadata(table: &str, batch: RecordBatch) -> Result<RecordBatch> {
    let schema = batch.schema();
    let arrays = schema
        .fields()
        .iter()
        .zip(batch.columns())
        .map(|(field, array)| -> Result<ArrayRef> {
            if matches!(table, "captures" | "recordings") && field.name() == "serial" {
                return Ok(new_null_array(field.data_type(), batch.num_rows()));
            }
            if !matches!(
                field.name().as_str(),
                "uri" | "source" | "path" | "remote_url" | "object_key"
            ) {
                return Ok(array.clone());
            }
            let clean = crate::tools::redact::for_model_text;
            Ok(match field.data_type() {
                DataType::Utf8 => Arc::new(StringArray::from_iter(
                    array
                        .as_any()
                        .downcast_ref::<StringArray>()
                        .context("invalid metadata string array")?
                        .iter()
                        .map(|value| value.map(clean)),
                )),
                DataType::LargeUtf8 => Arc::new(LargeStringArray::from_iter(
                    array
                        .as_any()
                        .downcast_ref::<LargeStringArray>()
                        .context("invalid large metadata string array")?
                        .iter()
                        .map(|value| value.map(clean)),
                )),
                DataType::Utf8View => Arc::new(StringViewArray::from_iter(
                    array
                        .as_any()
                        .downcast_ref::<StringViewArray>()
                        .context("invalid metadata string-view array")?
                        .iter()
                        .map(|value| value.map(clean)),
                )),
                _ => anyhow::bail!("source metadata column has an unsupported type"),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(RecordBatch::try_new(schema, arrays)?)
}

#[derive(Clone, Copy, Debug)]
pub struct SnapshotLimits {
    /// Combined rows across every base table. Exceeding a limit aborts the
    /// snapshot; the API never returns a silently incomplete database.
    pub max_rows: u64,
    /// Arrow array buffer bytes, conservatively counted for every batch.
    pub max_arrow_bytes: u64,
    pub max_batch_bytes: usize,
    pub timeout_ms: u64,
}
impl SnapshotLimits {
    pub(super) fn validate(self) -> Result<()> {
        ensure!(
            self.max_rows > 0 && self.max_rows <= 100_000_000,
            "snapshot row limit must be 1..100000000"
        );
        ensure!(
            self.max_arrow_bytes > 0,
            "snapshot transfer byte limit must be positive"
        );
        ensure!(
            self.max_batch_bytes > 0 && self.max_batch_bytes <= 256 * 1024 * 1024,
            "snapshot batch limit must be 1 byte to 256 MiB"
        );
        ensure!(
            self.timeout_ms > 0 && self.timeout_ms <= 300_000,
            "snapshot deadline must be 1..300000 milliseconds"
        );
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize)]
pub struct SnapshotReceipt {
    pub projection: SnapshotProjection,
    pub source_slot_id: Option<String>,
    pub source_dataset_id: String,
    pub rows: u64,
    pub arrow_bytes: u64,
    pub tables: BTreeMap<String, u64>,
}

/// A source borrow never outlives its reader or owning task writer. Its
/// connection is used only for fixed, schema-validated physical scans.
#[derive(Clone, Copy)]
pub(super) struct Source<'a> {
    conn: &'a Connection,
    dataset_id: &'a str,
    slot_id: Option<&'a str>,
    cancelled: Option<&'a AtomicBool>,
}
impl<'a> Source<'a> {
    pub(super) fn reader(reader: &'a Reader) -> Self {
        Self {
            conn: &reader.conn,
            dataset_id: &reader.status.dataset_id,
            slot_id: reader.status.slot_id.as_deref(),
            cancelled: Some(&reader.cancelled),
        }
    }
    pub(super) fn task(conn: &'a Connection, dataset_id: &'a str, slot_id: &'a str) -> Self {
        Self {
            conn,
            dataset_id,
            slot_id: Some(slot_id),
            cancelled: None,
        }
    }
    pub(super) fn check_cancelled(self) -> Result<()> {
        ensure!(
            self.cancelled
                .is_none_or(|flag| !flag.load(Ordering::Acquire)),
            "dataset operation cancelled because its open instance was closed"
        );
        Ok(())
    }
}

fn check(cancel: &AtomicBool, source: Option<Source<'_>>, deadline: Instant) -> Result<()> {
    ensure!(
        !cancel.load(Ordering::Acquire),
        "task dataset operation was cancelled"
    );
    if let Some(source) = source {
        source.check_cancelled()?;
    }
    ensure!(
        Instant::now() < deadline,
        "task dataset operation exceeded its deadline"
    );
    Ok(())
}

pub(super) fn with_watchdog<T>(
    cancel: &AtomicBool,
    source: Option<&Reader>,
    target: &Connection,
    deadline: Instant,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    with_source_watchdog(
        cancel,
        source.map(Source::reader),
        target,
        deadline,
        operation,
    )
}

pub(super) fn with_source_watchdog<T>(
    cancel: &AtomicBool,
    source: Option<Source<'_>>,
    target: &Connection,
    deadline: Instant,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    check(cancel, source, deadline)?;
    let source_cancelled = source.and_then(|source| source.cancelled);
    let source_interrupt = source.map(|source| source.conn.interrupt_handle());
    let target_interrupt = target.interrupt_handle();
    std::thread::scope(|scope| {
        let (done, receive) = mpsc::channel::<()>();
        let watcher = scope.spawn(move || {
            loop {
                if cancel.load(Ordering::Acquire)
                    || source_cancelled
                        .as_ref()
                        .is_some_and(|flag| flag.load(Ordering::Acquire))
                    || Instant::now() >= deadline
                {
                    if let Some(source) = &source_interrupt {
                        source.interrupt();
                    }
                    target_interrupt.interrupt();
                    break;
                }
                match receive.recv_timeout(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .min(Duration::from_millis(10)),
                ) {
                    Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                }
            }
        });
        let result = operation();
        drop(done);
        watcher
            .join()
            .map_err(|_| anyhow::anyhow!("task dataset deadline worker failed"))?;
        check(cancel, source, deadline)?;
        result
    })
}

struct Rollback<'a>(&'a Connection);
impl Drop for Rollback<'_> {
    fn drop(&mut self) {
        let _ = self.0.execute_batch("ROLLBACK");
    }
}

#[derive(PartialEq, Eq)]
struct ColumnSchema {
    index: u32,
    name: String,
    kind: String,
    nullable: bool,
    default: Option<String>,
}
#[derive(PartialEq, Eq)]
struct TableSchema {
    sql: String,
    columns: Vec<ColumnSchema>,
}

/// Introspection returns stored definitions without evaluating their defaults
/// or generated expressions. Fully qualified system functions cannot resolve
/// to a source catalog's user macro. No source SQL/definition is echoed.
fn table_schema(conn: &Connection, table: &str) -> Result<TableSchema> {
    let mut tables = conn.prepare("SELECT sql FROM system.main.duckdb_tables() WHERE database_name=system.main.current_database() AND schema_name='main' AND table_name=? AND NOT internal LIMIT 2")?;
    let mut rows = tables.query([table])?;
    let sql: String = rows
        .next()?
        .context("source catalog is missing an approved base table")?
        .get(0)?;
    ensure!(
        rows.next()?.is_none() && sql.len() <= 64 * 1024,
        "source catalog has an unsupported table schema"
    );
    let mut statement = conn.prepare("SELECT column_index,column_name,data_type,is_nullable,column_default FROM system.main.duckdb_columns() WHERE database_name=system.main.current_database() AND schema_name='main' AND table_name=? AND NOT internal LIMIT 129")?;
    let mut rows = statement.query([table])?;
    let mut columns = Vec::new();
    while let Some(row) = rows.next()? {
        let column = ColumnSchema {
            index: row.get(0)?,
            name: row.get(1)?,
            kind: row.get(2)?,
            nullable: row.get(3)?,
            default: row.get(4)?,
        };
        ensure!(
            columns.len() < 128
                && column.name.len() <= 1024
                && column.kind.len() <= 1024
                && column
                    .default
                    .as_ref()
                    .is_none_or(|value| value.len() <= 64 * 1024),
            "source catalog has an unsupported column schema"
        );
        columns.push(column);
    }
    columns.sort_by_key(|column| column.index);
    Ok(TableSchema { sql, columns })
}

pub(super) fn copy_into_with_hook(
    source: &Reader,
    target: &Connection,
    limits: SnapshotLimits,
    projection: SnapshotProjection,
    cancel: &AtomicBool,
    deadline: Instant,
    after_table: &mut dyn FnMut(&str) -> Result<()>,
) -> Result<SnapshotReceipt> {
    copy_source_into_with_hook(
        Source::reader(source),
        target,
        limits,
        projection,
        cancel,
        deadline,
        after_table,
    )
}

#[cfg(feature = "parquet")]
pub(super) fn copy_source_into(
    source: Source<'_>,
    target: &Connection,
    limits: SnapshotLimits,
    projection: SnapshotProjection,
    cancel: &AtomicBool,
    deadline: Instant,
) -> Result<SnapshotReceipt> {
    copy_source_into_with_hook(
        source,
        target,
        limits,
        projection,
        cancel,
        deadline,
        &mut |_| Ok(()),
    )
}

fn copy_source_into_with_hook(
    source: Source<'_>,
    target: &Connection,
    limits: SnapshotLimits,
    projection: SnapshotProjection,
    cancel: &AtomicBool,
    deadline: Instant,
    after_table: &mut dyn FnMut(&str) -> Result<()>,
) -> Result<SnapshotReceipt> {
    limits.validate()?;
    // Keep cleanup outside the watched operation. An interrupt racing a
    // ROLLBACK can leave the borrowed source in an aborted transaction, so the
    // watcher must have stopped and joined before either guard runs.
    let mut source_transaction = None;
    let mut target_transaction = None;
    let result = with_source_watchdog(cancel, Some(source), target, deadline, || {
        // Even an interrupted BEGIN may have entered a transaction before
        // reporting failure. These connections have no caller-owned transaction.
        source_transaction = Some(Rollback(source.conn));
        source.conn.execute_batch("BEGIN TRANSACTION READ ONLY")?;
        target_transaction = Some(Rollback(target));
        target.execute_batch("BEGIN TRANSACTION")?;
        target.execute_batch(crate::dataset::db::DDL)?;

        // Validate every table before scanning any row. Names alone would
        // admit generated columns, whose expressions could read source engine
        // settings or perform expensive work in otherwise preserved values.
        for table in BASE_TABLES {
            check(cancel, Some(source), deadline)?;
            ensure!(
                table_schema(source.conn, table)? == table_schema(target, table)?,
                "source catalog has an unsupported table or column schema"
            );
        }
        let mut receipt = SnapshotReceipt {
            projection,
            source_slot_id: source.slot_id.map(str::to_owned),
            source_dataset_id: source.dataset_id.to_owned(),
            rows: 0,
            arrow_bytes: 0,
            tables: BTreeMap::new(),
        };
        for table in BASE_TABLES {
            check(cancel, Some(source), deadline)?;
            // Only fixed physical scans enter the shared engine. Joins, sorts,
            // ASOF evaluation and arbitrary user SQL run in the target only.
            let sql = format!(
                "SELECT * FROM main.\"{table}\" LIMIT {}",
                limits
                    .max_rows
                    .saturating_sub(receipt.rows)
                    .saturating_add(1)
            );
            let mut scan = source.conn.prepare(&sql)?;
            let _ = scan.stream_arrow([])?;
            let mut append = target.appender(table)?;
            let mut count = 0;
            loop {
                check(cancel, Some(source), deadline)?;
                let Some(array) = scan
                    .step()
                    .with_context(|| format!("failed to fetch source table {table}"))?
                else {
                    break;
                };
                let batch = RecordBatch::from(&array);
                let bytes = batch.get_array_memory_size();
                let rows = u64::try_from(batch.num_rows())?;
                ensure!(
                    bytes <= limits.max_batch_bytes,
                    "source snapshot batch exceeds its transfer byte limit"
                );
                ensure!(
                    rows <= limits.max_rows.saturating_sub(receipt.rows),
                    "source snapshot exceeds its row limit"
                );
                let bytes = u64::try_from(bytes)?;
                ensure!(
                    bytes <= limits.max_arrow_bytes.saturating_sub(receipt.arrow_bytes),
                    "source snapshot exceeds its transfer byte limit"
                );
                check(cancel, Some(source), deadline)?;
                let batch = if projection == SnapshotProjection::Model {
                    project_model_metadata(table, batch)?
                } else {
                    batch
                };
                append.append_record_batch(batch)?;
                receipt.rows += rows;
                receipt.arrow_bytes += bytes;
                count += rows;
            }
            append.flush()?;
            drop(append);
            drop(scan);
            receipt.tables.insert(table.into(), count);
            after_table(table)?;
        }
        check(cancel, Some(source), deadline)?;
        target.execute_batch("COMMIT")?;
        check(cancel, Some(source), deadline)?;
        Ok(receipt)
    });
    drop(target_transaction);
    drop(source_transaction);
    check(cancel, Some(source), deadline)?;
    result
}
