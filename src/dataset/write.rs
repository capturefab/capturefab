//! Typed SQL shared by the session catalog and isolated task writers.
//! Callers own transactions, quotas, lifecycle state and published counters.
use crate::{
    context::Context,
    signals::{Sample, SignalInfo, TagInfo, Value as SignalValue},
};
use anyhow::Result;
use duckdb::{Connection, params};
use std::collections::BTreeMap;

pub(super) type SampleRanges<'a> = BTreeMap<&'a str, (u64, u64, &'static str)>;

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

pub(super) fn protocol(source: &SignalInfo) -> Result<String> {
    Ok(serde_json::to_value(source.protocol)?
        .as_str()
        .unwrap_or_default()
        .to_owned())
}

pub(super) fn register(
    conn: &Connection,
    source: &SignalInfo,
    tags: &[TagInfo],
    protocol: &str,
) -> Result<()> {
    for tag in tags {
        conn.execute(
            "INSERT INTO signals (source_id, protocol, uri, tag, unit, kind) VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT (source_id, tag) DO UPDATE SET protocol = excluded.protocol, uri = excluded.uri,
               unit = coalesce(excluded.unit, signals.unit), kind = coalesce(excluded.kind, signals.kind)",
            params![source.id,protocol,source.uri,tag.tag,tag.unit,tag.kind],
        )?;
    }
    Ok(())
}

pub(super) fn sample_ranges(batch: &[Sample]) -> SampleRanges<'_> {
    let mut range = BTreeMap::new();
    for sample in batch {
        let kind = match sample.value {
            SignalValue::Bool(_) => "bool",
            SignalValue::Number(_) => "number",
            SignalValue::Text(_) => "text",
        };
        let entry = range
            .entry(sample.tag.as_str())
            .or_insert((sample.ts_ms, sample.ts_ms, kind));
        entry.0 = entry.0.min(sample.ts_ms);
        entry.1 = entry.1.max(sample.ts_ms);
    }
    range
}

pub(super) fn append_samples(
    conn: &Connection,
    source_id: &str,
    uri: &str,
    protocol: &str,
    batch: &[Sample],
    range: &SampleRanges<'_>,
) -> Result<()> {
    {
        let mut appender = conn.appender("samples")?;
        for sample in batch {
            let (double, text) = match &sample.value {
                SignalValue::Bool(value) => (
                    Some(if *value { 1.0 } else { 0.0 }),
                    Some(if *value { "true" } else { "false" }.to_owned()),
                ),
                SignalValue::Number(value) => (Some(*value), None),
                SignalValue::Text(value) => (None, Some(value.clone())),
            };
            let quality = serde_json::to_value(sample.quality)?
                .as_str()
                .map(str::to_owned);
            appender.append_row(params![
                sample.ts_ms,
                sample.source_ts_ms,
                source_id,
                sample.tag,
                double,
                text,
                quality
            ])?;
        }
        appender.flush()?;
    }
    for (tag, (first, last, kind)) in range {
        conn.execute(
            "INSERT INTO signals (source_id, protocol, uri, tag, kind, first_ms, last_ms)
             VALUES (?, nullif(?, ''), ?, ?, ?, ?, ?)
             ON CONFLICT (source_id, tag) DO UPDATE SET
               kind = coalesce(signals.kind, excluded.kind),
               first_ms = least(coalesce(signals.first_ms, excluded.first_ms), excluded.first_ms),
               last_ms = greatest(coalesce(signals.last_ms, excluded.last_ms), excluded.last_ms)",
            params![source_id, protocol, uri, tag, kind, first, last],
        )?;
    }
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
