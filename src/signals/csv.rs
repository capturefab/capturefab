//! `log --csv`: samples as RFC 4180 CSV through the storage ledger, so the
//! file counts against the storage budget and is never overwritten.
use super::{Quality, Sample, SampleSink, SignalInfo, TagInfo, Value};
use crate::storage::{StoragePolicy, StorageWriter};
use anyhow::{Context, Result};
use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

pub const HEADER: &str = "ts_ms,source_ts_ms,source_id,tag,value,quality\n";

pub struct CsvSink {
    path: PathBuf,
    writer: Mutex<Option<StorageWriter>>,
    rows: AtomicU64,
    /// Remove the file at finish when it holds no rows.
    discard_empty: AtomicBool,
}

impl CsvSink {
    /// Create a new CSV file reserving at most `max_bytes` of the budget.
    pub fn create(path: &Path, policy: &StoragePolicy, max_bytes: u64) -> Result<Self> {
        let mut writer =
            crate::storage::create_writer(path, policy, max_bytes.min(policy.max_bytes))?;
        writer.write_all(HEADER.as_bytes())?;
        Ok(Self {
            path: path.to_path_buf(),
            writer: Mutex::new(Some(writer)),
            rows: AtomicU64::new(0),
            discard_empty: AtomicBool::new(false),
        })
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    /// A run that failed before logging anything leaves no header-only file
    /// behind, so the same command can be run again.
    pub fn discard_if_empty(&self) {
        self.discard_empty.store(true, Ordering::SeqCst);
    }
}

fn field(text: &str) -> String {
    if text.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", text.replace('"', "\"\""))
    } else {
        text.to_string()
    }
}

/// One CSV row, newline included.
pub fn row(source_id: &str, sample: &Sample) -> String {
    let value = match &sample.value {
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Text(t) => t.clone(),
    };
    let quality = match sample.quality {
        Quality::Good => "good",
        Quality::Uncertain => "uncertain",
        Quality::Bad => "bad",
    };
    format!(
        "{},{},{},{},{},{quality}\n",
        sample.ts_ms,
        sample
            .source_ts_ms
            .map(|t| t.to_string())
            .unwrap_or_default(),
        field(source_id),
        field(&sample.tag),
        field(&value),
    )
}

impl SampleSink for CsvSink {
    fn register(&self, _source: &SignalInfo, _tags: &[TagInfo]) -> Result<()> {
        Ok(())
    }
    fn write(&self, source_id: &str, batch: &[Sample]) -> Result<()> {
        let mut text = String::new();
        for sample in batch {
            text.push_str(&row(source_id, sample));
        }
        let mut writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        let writer = writer.as_mut().context("CSV file is already finished")?;
        writer.write_all(text.as_bytes())?;
        self.rows.fetch_add(batch.len() as u64, Ordering::Relaxed);
        Ok(())
    }
    fn finish(&self) -> Result<()> {
        let writer = self.writer.lock().unwrap_or_else(|e| e.into_inner()).take();
        match writer {
            Some(writer)
                if self.discard_empty.load(Ordering::SeqCst)
                    && self.rows.load(Ordering::Relaxed) == 0 =>
            {
                writer.abort()
            }
            Some(mut writer) => {
                writer.flush()?;
                writer.finish()
            }
            None => Ok(()),
        }
    }
    fn describe(&self) -> String {
        format!(
            "CSV {}",
            self.path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_fields() {
        let mut sample = Sample::now("a,\"b\"", Value::Text("x\ny".into()), Quality::Bad);
        sample.ts_ms = 5;
        sample.source_ts_ms = Some(4);
        assert_eq!(
            row("signal:1", &sample),
            "5,4,signal:1,\"a,\"\"b\"\"\",\"x\ny\",bad\n"
        );
        let mut sample = Sample::now("t", Value::Bool(true), Quality::Good);
        sample.ts_ms = 1;
        assert_eq!(row("s", &sample), "1,,s,t,true,good\n");
    }
}
