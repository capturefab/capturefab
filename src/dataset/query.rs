//! Read-only access to a dataset: a cloned connection that runs one
//! SELECT/WITH statement or an export, outside the writer's lock.
use super::db::{Staging, explain, file_label};
use anyhow::{Result, bail, ensure};
use base64::Engine as _;
use duckdb::{Connection, InterruptHandle, types::Value as Db};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Condvar, Mutex, PoisonError, mpsc},
    time::{Duration, Instant},
};

pub const MAX_SQL_BYTES: usize = 64 * 1024;
pub const MAX_LIMIT: u32 = 10_000;
pub const MAX_TIMEOUT_MS: u64 = 300_000;
pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;
/// Query results stop growing past this many bytes of JSON.
pub const MAX_RESULT_BYTES: usize = 8 * 1024 * 1024;

/// Outstanding readers of the session's dataset, so closing can interrupt them.
#[derive(Default)]
pub(crate) struct Readers {
    state: Mutex<(u64, BTreeMap<u64, Arc<InterruptHandle>>)>,
    idle: Condvar,
}
impl Readers {
    fn add(self: &Arc<Self>, handle: Arc<InterruptHandle>) -> ReaderGuard {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.0 += 1;
        let id = state.0;
        state.1.insert(id, handle);
        ReaderGuard {
            readers: self.clone(),
            id,
        }
    }
    /// Interrupt every reader and wait until they finish, at most `wait`.
    pub fn interrupt_and_wait(&self, wait: Duration) -> bool {
        let deadline = Instant::now() + wait;
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        for handle in state.1.values() {
            handle.interrupt();
        }
        while !state.1.is_empty() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            state = self
                .idle
                .wait_timeout(state, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
            for handle in state.1.values() {
                handle.interrupt();
            }
        }
        true
    }
}
pub(crate) struct ReaderGuard {
    readers: Arc<Readers>,
    id: u64,
}
impl Drop for ReaderGuard {
    fn drop(&mut self) {
        let mut state = self
            .readers
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        state.1.remove(&self.id);
        self.readers.idle.notify_all();
    }
}

/// A connection for queries and exports. In a session it is a clone of the
/// writer's connection, so it sees a consistent snapshot while inserts go on.
pub struct Reader {
    pub(crate) conn: Connection,
    pub(crate) staging: Arc<Staging>,
    pub(crate) path: PathBuf,
    pub(crate) status: super::DatasetStatus,
    /// Opened with DuckDB's read-only access mode.
    pub(crate) read_only: bool,
    _guard: Option<ReaderGuard>,
}

impl super::Dataset {
    pub(crate) fn reader_with(&self, readers: Option<&Arc<Readers>>) -> Result<Reader> {
        let conn = self
            .conn
            .try_clone()
            .map_err(|e| explain(e, &self.path, "read"))?;
        let guard = readers.map(|r| r.add(conn.interrupt_handle()));
        Ok(Reader {
            conn,
            staging: self.staging.clone(),
            path: self.path.clone(),
            status: self.status.clone(),
            read_only: self.read_only,
            _guard: guard,
        })
    }
    /// A reader for a dataset opened with `open_read_only`.
    pub fn reader(&self) -> Result<Reader> {
        self.reader_with(None)
    }
}

/// Run `f` with a watchdog that interrupts the connection at the deadline.
pub(crate) fn with_timeout<T>(
    conn: &Connection,
    timeout: Duration,
    f: impl FnOnce() -> Result<T>,
) -> (Result<T>, bool) {
    let handle = conn.interrupt_handle();
    let (done, wait) = mpsc::channel::<()>();
    let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = fired.clone();
    let watchdog = std::thread::spawn(move || {
        if let Err(mpsc::RecvTimeoutError::Timeout) = wait.recv_timeout(timeout) {
            flag.store(true, std::sync::atomic::Ordering::Release);
            handle.interrupt();
        }
    });
    let result = f();
    drop(done);
    let _ = watchdog.join();
    (result, fired.load(std::sync::atomic::Ordering::Acquire))
}

impl Reader {
    /// Refuse anything but exactly one SELECT/WITH statement, before DuckDB
    /// prepares it: preparing runs every statement but the last.
    fn guard(&self, sql: &str) -> Result<()> {
        if !cfg!(feature = "parquet") {
            // Without the json extension there is no parser check; only a
            // read-only connection is safe then.
            ensure!(
                self.read_only,
                "dataset query in a session unsupported in this build; use dataset query --file"
            );
            return Ok(());
        }
        let parsed: String = self
            .conn
            .query_row(
                "SELECT json_serialize_sql(CAST(? AS VARCHAR))",
                [sql],
                |r| r.get(0),
            )
            .map_err(|e| explain(e, &self.path, "query"))?;
        let parsed: Value = serde_json::from_str(&parsed)?;
        let one = parsed["error"] == false
            && parsed["statements"]
                .as_array()
                .is_some_and(|s| s.len() == 1);
        if !one {
            return Err(
                super::Usage("dataset query accepts one SELECT/WITH statement".into()).into(),
            );
        }
        Ok(())
    }

    pub fn query(&self, sql: &str, limit: u32, timeout_ms: u64) -> Result<Value> {
        ensure!(
            sql.len() <= MAX_SQL_BYTES,
            "dataset query must be at most 64 KiB"
        );
        ensure!(
            (1..=MAX_LIMIT).contains(&limit),
            "dataset query limit must be 1..10000"
        );
        ensure!(
            timeout_ms <= MAX_TIMEOUT_MS,
            "dataset query timeout must be at most 300000 milliseconds"
        );
        let timeout_ms = if timeout_ms == 0 {
            DEFAULT_TIMEOUT_MS
        } else {
            timeout_ms
        };
        self.guard(sql)?;
        self.conn
            .execute_batch("BEGIN TRANSACTION")
            .map_err(|e| explain(e, &self.path, "query"))?;
        let (result, fired) = with_timeout(&self.conn, Duration::from_millis(timeout_ms), || {
            self.run(sql, limit)
        });
        let _ = self.conn.execute_batch("ROLLBACK");
        match result {
            Ok(value) => Ok(value),
            Err(e) if fired || e.to_string().contains("INTERRUPT") => {
                bail!("dataset query timed out after {timeout_ms} ms")
            }
            Err(e) => Err(e),
        }
    }

    fn run(&self, sql: &str, limit: u32) -> Result<Value> {
        let mut statement = self
            .conn
            .prepare(sql)
            .map_err(|e| explain(e, &self.path, "query"))?;
        let mut rows = statement
            .query([])
            .map_err(|e| explain(e, &self.path, "query"))?;
        let columns: Vec<Value> = rows
            .as_ref()
            .map(|stmt| {
                (0..stmt.column_count())
                    .map(|i| {
                        json!({
                            "name": stmt.column_name(i).map(String::as_str).unwrap_or(""),
                            "type": format!("{}", stmt.column_type(i)),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mut out = Vec::new();
        let mut bytes = 0usize;
        let mut truncated = false;
        while let Some(row) = rows.next().map_err(|e| explain(e, &self.path, "query"))? {
            if out.len() >= limit as usize {
                truncated = true;
                break;
            }
            let values: Vec<Value> = (0..columns.len())
                .map(|i| row.get::<_, Db>(i).map(to_json))
                .collect::<duckdb::Result<_>>()?;
            let value = Value::Array(values);
            bytes += value.to_string().len();
            if bytes > MAX_RESULT_BYTES {
                truncated = true;
                break;
            }
            out.push(value);
        }
        Ok(json!({
            "columns": columns,
            "row_count": out.len(),
            "rows": out,
            "truncated": truncated,
        }))
    }

    pub fn label(&self) -> String {
        file_label(&self.path)
    }
}

const SAFE_INTEGER: i128 = 1 << 53;
fn integer(value: i128) -> Value {
    if (-SAFE_INTEGER..=SAFE_INTEGER).contains(&value) {
        json!(value as i64)
    } else {
        json!(value.to_string())
    }
}

/// DuckDB values as JSON: large integers and decimals as strings, times as
/// ISO 8601 text and blobs as base64.
pub fn to_json(value: Db) -> Value {
    use duckdb::types::TimeUnit;
    let micros = |unit: TimeUnit, v: i64| -> i64 {
        match unit {
            TimeUnit::Second => v.saturating_mul(1_000_000),
            TimeUnit::Millisecond => v.saturating_mul(1000),
            TimeUnit::Microsecond => v,
            TimeUnit::Nanosecond => v / 1000,
        }
    };
    match value {
        Db::Null => Value::Null,
        Db::Boolean(b) => json!(b),
        Db::TinyInt(v) => json!(v),
        Db::SmallInt(v) => json!(v),
        Db::Int(v) => json!(v),
        Db::BigInt(v) => integer(v.into()),
        Db::HugeInt(v) => integer(v),
        Db::UHugeInt(v) => i128::try_from(v).map_or_else(|_| json!(v.to_string()), integer),
        Db::UTinyInt(v) => json!(v),
        Db::USmallInt(v) => json!(v),
        Db::UInt(v) => json!(v),
        Db::UBigInt(v) => integer(v.into()),
        Db::Float(v) => serde_json::Number::from_f64(v.into()).map_or(Value::Null, Value::Number),
        Db::Double(v) => serde_json::Number::from_f64(v).map_or(Value::Null, Value::Number),
        Db::Decimal(d) => json!(d.to_string()),
        Db::Timestamp(unit, v) => chrono::DateTime::from_timestamp_micros(micros(unit, v))
            .map_or(Value::Null, |t| {
                json!(t.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true))
            }),
        Db::Text(s) | Db::Enum(s) => json!(s),
        Db::Blob(b) | Db::Geometry(b) => json!(base64::engine::general_purpose::STANDARD.encode(b)),
        Db::Date32(days) => chrono::NaiveDate::from_ymd_opt(1970, 1, 1)
            .and_then(|d| d.checked_add_signed(chrono::TimeDelta::days(days.into())))
            .map_or(Value::Null, |d| json!(d.to_string())),
        Db::Time64(unit, v) => {
            let us = micros(unit, v);
            chrono::NaiveTime::from_num_seconds_from_midnight_opt(
                (us / 1_000_000) as u32,
                ((us % 1_000_000) * 1000) as u32,
            )
            .map_or(Value::Null, |t| json!(t.to_string()))
        }
        Db::Interval {
            months,
            days,
            nanos,
        } => json!({"months": months, "days": days, "nanos": nanos}),
        Db::List(items) | Db::Array(items) => {
            Value::Array(items.into_iter().map(to_json).collect())
        }
        Db::Struct(fields) => Value::Object(
            fields
                .iter()
                .map(|(k, v)| (k.clone(), to_json(v.clone())))
                .collect(),
        ),
        Db::Map(entries) => Value::Object(
            entries
                .iter()
                .map(|(k, v)| {
                    let key = match to_json(k.clone()) {
                        Value::String(s) => s,
                        other => other.to_string(),
                    };
                    (key, to_json(v.clone()))
                })
                .collect(),
        ),
        Db::Union(inner) => to_json(*inner),
        other => json!(format!("{other:?}")),
    }
}
