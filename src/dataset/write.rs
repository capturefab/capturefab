//! Typed SQL shared by the session catalog and isolated task writers.
//! Callers own transactions, quotas, lifecycle state and published counters.
use crate::context::Context;
use anyhow::Result;
use duckdb::{Connection, params};

pub(super) fn initialize(
    conn: &Connection,
    id: &str,
    name: &str,
    created_ms: u64,
    max_bytes: u64,
) -> Result<()> {
    conn.execute_batch(super::db::DDL)?;
    conn.execute(
        "INSERT INTO dataset_info (singleton, schema_version, dataset_id, name, created_ms, generator, max_bytes) VALUES (1, ?, ?, ?, ?, ?, ?)",
        params![super::SCHEMA_VERSION,id,name,created_ms,format!("capturefab {}",env!("CARGO_PKG_VERSION")),max_bytes],
    )?;
    Ok(())
}

pub(super) fn record_event(
    conn: &Connection,
    wall_ms: u64,
    kind: &str,
    camera_id: Option<&str>,
    source_id: Option<&str>,
    text: &str,
    context: &Context,
) -> duckdb::Result<u64> {
    conn.query_row(
        "INSERT INTO events (wall_ms, kind, camera_id, source_id, text, context) VALUES (?, ?, ?, ?, ?, ?) RETURNING id",
        params![wall_ms,kind,camera_id,source_id,text,context_json(context)],
        |row| row.get(0),
    )
}

pub(super) fn context_json(context: &Context) -> Option<String> {
    (!context.is_empty()).then(|| serde_json::to_string(context).unwrap_or_default())
}
