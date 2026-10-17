//! The coordinator's signal hub: starts and stops source helpers, keeps the
//! latest values per tag and hands samples to the attached `SampleSink`.
use super::{Sample, SampleSink, SourceStatus};
use crate::session::{LogEntry, SessionCommand};
use anyhow::{Result, bail};
use serde_json::Value;
use std::{collections::BTreeMap, sync::Arc, time::Duration};

#[derive(Default)]
pub struct SignalHub {}

impl SignalHub {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn start(
        &self,
        _uri: &str,
        _interval_ms: u64,
        _tags: Vec<String>,
        _timeout_ms: u64,
    ) -> Result<SourceStatus> {
        bail!("signal logging is not implemented yet")
    }
    /// Stop a source by ID, or every source with "all".
    pub fn stop(&self, _id: &str, _wait: Duration) -> Result<Vec<SourceStatus>> {
        bail!("signal logging is not implemented yet")
    }
    pub fn stop_all(&self, _wait: Duration) {}
    /// Ask every helper to stop without waiting; used from `Drop`.
    pub fn stop_all_nowait(&self) {}
    /// Up to 256 tags per source; recent points only with `history`.
    pub fn status(&self, _history: bool) -> Vec<SourceStatus> {
        Vec::new()
    }
    /// Up to 32 tags per source and no history, for session snapshots.
    pub fn status_brief(&self) -> Vec<SourceStatus> {
        Vec::new()
    }
    /// Attach or detach the sink after the batches already queued.
    pub fn set_sink(&self, _sink: Option<Arc<dyn SampleSink>>, _timeout: Duration) -> Result<()> {
        Ok(())
    }
    pub fn has_sink(&self) -> bool {
        false
    }
    pub fn sink_error(&self) -> Option<String> {
        None
    }
    pub fn dropped_total(&self) -> u64 {
        0
    }
    pub fn flush(&self, _timeout: Duration) -> Result<()> {
        Ok(())
    }
    /// Latest sample per `source_id/tag`.
    pub fn latest_values(&self) -> BTreeMap<String, Sample> {
        BTreeMap::new()
    }
    pub fn logs(&self) -> Vec<LogEntry> {
        Vec::new()
    }
}

/// Coordinator arm for `signals`, `signal_read`, `signal_browse`,
/// `signal_start` and `signal_stop`.
pub fn run(_hub: &SignalHub, command: SessionCommand) -> Result<Value> {
    match command {
        SessionCommand::Signals
        | SessionCommand::SignalRead { .. }
        | SessionCommand::SignalBrowse { .. }
        | SessionCommand::SignalStart { .. }
        | SessionCommand::SignalStop { .. } => bail!("signals are not implemented yet"),
        _ => bail!("not a signal command"),
    }
}
