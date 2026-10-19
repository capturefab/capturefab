//! Portable, private model queries over projected base-table snapshots.
//!
//! The shared copier validates physical source schemas before projecting rows.
//! This boundary exposes neither a Connection nor a general SQL execution
//! method. The in-memory engine cannot spill, attach a
//! source, load extensions or read persistent secrets. Transfer/result limits
//! and DuckDB's memory setting are not a process RSS or shared-source disk cap.
#[cfg(feature = "parquet")]
use super::query::ConnectionQuery;
use super::{Reader, snapshot};
#[cfg(feature = "parquet")]
use anyhow::Context as _;
use anyhow::{Result, ensure};
#[cfg(feature = "parquet")]
use duckdb::{Config, Connection};
use serde_json::Value;
use std::sync::atomic::AtomicBool;
#[cfg(feature = "parquet")]
use std::{path::Path, time::Instant};

#[cfg(feature = "parquet")]
const INERT_DIRECTORY: &str = "__capturefab_model_private__";
#[cfg(feature = "parquet")]
const MEMORY_LIMIT: &str = "32MiB";
#[cfg(feature = "parquet")]
const SETUP: &str = "
    SET allowed_paths = [];
    SET allowed_directories = [];
    SET extension_directories = [];
    SET http_proxy = '';
    SET http_proxy_username = '';
    SET http_proxy_password = '';
    SET username = '';
    SET password = '';
    SET enable_external_access = false;
    SET lock_configuration = true;
";

#[derive(Clone, Copy, Debug)]
pub(crate) struct ModelQueryLimits {
    /// These are transfer limits, except timeout_ms: that is one absolute
    /// budget for configuration/open, the complete copy and the final query.
    pub snapshot: snapshot::SnapshotLimits,
    pub max_result_rows: u32,
    pub max_result_bytes: usize,
}
impl ModelQueryLimits {
    pub(super) fn bounded(rows: u32, timeout_ms: u64, bytes: usize) -> Result<Self> {
        ensure!(
            timeout_ms <= super::query::MAX_TIMEOUT_MS,
            "dataset query timeout must be at most 300000 milliseconds"
        );
        Ok(Self {
            snapshot: snapshot::SnapshotLimits {
                max_rows: 100_000,
                max_arrow_bytes: 16 * 1024 * 1024,
                max_batch_bytes: 8 * 1024 * 1024,
                timeout_ms: if timeout_ms == 0 {
                    super::query::DEFAULT_TIMEOUT_MS
                } else {
                    timeout_ms
                },
            },
            max_result_rows: rows,
            max_result_bytes: bytes,
        })
    }
}

// A private owner: no connection or clone can escape before its database drops.
#[cfg(feature = "parquet")]
struct ModelSnapshot {
    conn: Connection,
    _receipt: snapshot::SnapshotReceipt,
}

#[cfg(feature = "parquet")]
fn check(cancel: &AtomicBool, source: snapshot::Source<'_>, deadline: Instant) -> Result<()> {
    use std::sync::atomic::Ordering;
    ensure!(
        !cancel.load(Ordering::Acquire),
        "model dataset query was cancelled"
    );
    source.check_cancelled()?;
    ensure!(
        Instant::now() < deadline,
        "model dataset query exceeded its deadline"
    );
    Ok(())
}

#[cfg(feature = "parquet")]
fn remaining_query_ms(deadline: Instant) -> Result<u64> {
    let remaining = deadline
        .saturating_duration_since(Instant::now())
        .as_millis();
    // Passing zero would make ConnectionQuery select its default timeout.
    ensure!(remaining > 0, "model dataset query exceeded its deadline");
    Ok(u64::try_from(remaining)?)
}

#[cfg(feature = "parquet")]
fn configuration() -> Result<Config> {
    Ok(Config::default()
        // Both must be set before open: an empty unset temp directory otherwise
        // becomes a cwd/.tmp path, and a later no-spill SET is too late.
        .with("temp_directory", "")?
        .with("max_temp_directory_size", "0B")?
        .max_memory(MEMORY_LIMIT)?
        .threads(2)?
        .enable_autoload_extension(false)?
        .with("allow_community_extensions", "false")?
        .with("allow_unsigned_extensions", "false")?
        .with("allow_persistent_secrets", "false")?
        .with("allow_unredacted_secrets", "false")?
        .with("secret_directory", INERT_DIRECTORY)?
        .with("home_directory", INERT_DIRECTORY)?
        .with("extension_directory", INERT_DIRECTORY)?
        .with("http_proxy", "capturefab-model.invalid")?
        .with("http_proxy_username", "")?
        .with("http_proxy_password", "")?
        .with("username", "")?
        .with("password", "")?
        // Keep this enabled only through fixed setup SQL. Opening ':memory:'
        // with external=false otherwise grants literal ':memory:.wal' paths.
        // No user SQL or source data is evaluated before SETUP closes access.
        .enable_external_access(true)?)
}

#[cfg(feature = "parquet")]
fn open_private() -> Result<Connection> {
    Connection::open_in_memory_with_flags(configuration()?)
        .context("open private model snapshot engine")
}

#[cfg(feature = "parquet")]
fn seal(conn: &Connection) -> Result<()> {
    conn.execute_batch(SETUP)?;
    // Refuse to proceed if a changed engine stops honoring a security setting.
    let safe: bool = conn.query_row(
        "SELECT current_setting('temp_directory') = ''
            AND current_setting('max_temp_directory_size') = '0 bytes'
            AND current_setting('allowed_paths') = []::VARCHAR[]
            AND current_setting('allowed_directories') = []::VARCHAR[]
            AND current_setting('extension_directories') = []::VARCHAR[]
            AND NOT current_setting('enable_external_access')
            AND current_setting('lock_configuration')
            AND NOT current_setting('autoload_known_extensions')
            AND NOT current_setting('autoinstall_known_extensions')
            AND NOT current_setting('allow_persistent_secrets')
            AND NOT current_setting('allow_unredacted_secrets')
            AND current_setting('http_proxy') = ''
            AND current_setting('http_proxy_username') = ''
            AND current_setting('http_proxy_password') = ''
            AND current_setting('username') = ''
            AND current_setting('password') = ''
            AND current_setting('secret_directory') = ?
            AND current_setting('home_directory') = ?
            AND current_setting('extension_directory') = ?",
        [INERT_DIRECTORY; 3],
        |row| row.get(0),
    )?;
    ensure!(
        safe,
        "private model snapshot configuration was not enforced"
    );
    Ok(())
}

#[cfg(feature = "parquet")]
pub(crate) fn query(
    source: &Reader,
    sql: &str,
    limits: ModelQueryLimits,
    cancel: &AtomicBool,
) -> Result<Value> {
    query_source(snapshot::Source::reader(source), sql, limits, cancel)
}

#[cfg(feature = "parquet")]
pub(super) fn query_source(
    source: snapshot::Source<'_>,
    sql: &str,
    limits: ModelQueryLimits,
    cancel: &AtomicBool,
) -> Result<Value> {
    // Start before any engine/configuration work. A non-preemptible open may
    // finish late, but a late operation can never be acknowledged as success.
    let started = Instant::now();
    limits.snapshot.validate()?;
    ensure!(
        !sql.trim().is_empty() && sql.len() <= super::query::MAX_SQL_BYTES,
        "dataset query must be 1 byte to 64 KiB of SQL"
    );
    ensure!(
        (1..=super::query::MAX_LIMIT).contains(&limits.max_result_rows),
        "dataset query limit must be 1..10000"
    );
    ensure!(
        (super::query::MIN_RESULT_BYTES..=super::query::MAX_RESULT_BYTES)
            .contains(&limits.max_result_bytes),
        "dataset query result limit must be 256..8388608 bytes"
    );
    let deadline = started + std::time::Duration::from_millis(limits.snapshot.timeout_ms);
    check(cancel, source, deadline)?;
    let conn = open_private()?;
    check(cancel, source, deadline)?;
    snapshot::with_source_watchdog(cancel, Some(source), &conn, deadline, || {
        seal(&conn)?;
        check(cancel, source, deadline)
    })?;
    // The copier owns its watcher and joins it before rolling back the source
    // snapshot. Never wrap that cleanup in another interrupting watcher.
    let receipt = snapshot::copy_source_into(
        source,
        &conn,
        limits.snapshot,
        snapshot::SnapshotProjection::Model,
        cancel,
        deadline,
    )?;
    let model = ModelSnapshot {
        conn,
        _receipt: receipt,
    };
    // No user SQL runs until every copied row has undergone Model projection
    // and the copy has committed. The one query watcher joins before cleanup.
    let reply = ConnectionQuery {
        conn: &model.conn,
        path: Path::new(":memory:"),
        read_only: false,
        cancelled: cancel,
    }
    .query_bounded_with_watchdog(
        sql,
        limits.max_result_rows,
        remaining_query_ms(deadline)?,
        limits.max_result_bytes,
        |_, operation| {
            snapshot::with_source_watchdog(cancel, Some(source), &model.conn, deadline, operation)
        },
    )?;
    check(cancel, source, deadline)?;
    Ok(reply)
}

#[cfg(not(feature = "parquet"))]
pub(crate) fn query(
    source: &Reader,
    sql: &str,
    limits: ModelQueryLimits,
    cancel: &AtomicBool,
) -> Result<Value> {
    query_source(snapshot::Source::reader(source), sql, limits, cancel)
}

#[cfg(not(feature = "parquet"))]
pub(super) fn query_source(
    _source: snapshot::Source<'_>,
    _sql: &str,
    limits: ModelQueryLimits,
    _cancel: &AtomicBool,
) -> Result<Value> {
    let _ = (
        limits.snapshot,
        limits.max_result_rows,
        limits.max_result_bytes,
    );
    anyhow::bail!("projected model dataset queries are unsupported in this build")
}

#[cfg(all(test, feature = "parquet"))]
#[path = "model_snapshot_tests.rs"]
mod tests;

#[cfg(all(test, not(feature = "parquet")))]
mod unsupported_tests {
    #[test]
    fn reader_model_query_refuses_without_parser_and_never_uses_raw_source() {
        use crate::dataset::{OpenMode, Slot};
        let root = std::env::temp_dir().join(format!(
            "capturefab-model-unsupported-{}-{}",
            std::process::id(),
            crate::dataset::db::random_name().unwrap()
        ));
        crate::ipc::ensure_private_dir(&root).unwrap();
        let slot = Slot::new();
        let status = slot
            .open(&root.join("source.duckdb"), OpenMode::Create, None)
            .unwrap();
        let reader = slot.reader_if(status.slot_id.as_deref().unwrap()).unwrap();
        let error = reader
            .query_model_bounded("SELECT 42", 1, 1000, 1024)
            .unwrap_err();
        assert!(error.to_string().contains("unsupported"), "{error:#}");
        assert_eq!(
            reader
                .conn
                .query_row("SELECT 42", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            42
        );
        drop(reader);
        slot.close().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
}
