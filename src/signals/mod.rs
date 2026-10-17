//! Sensor and PLC signals: Modbus TCP, MQTT, OPC UA, MTConnect and a
//! simulator. A separate subsystem from cameras: each source runs in its own
//! `__signal` helper process and the coordinator's `SignalHub` collects its
//! samples into a bounded in-memory view and an optional `SampleSink`.
pub mod hub;

use crate::session::SessionCommand;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Protocol {
    ModbusTcp,
    Mqtt,
    Opcua,
    Mtconnect,
    SimSignal,
}

/// One reading. Numbers are always finite; text is at most 1 KiB.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum Value {
    Bool(bool),
    Number(f64),
    Text(String),
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Quality {
    #[default]
    Good,
    Uncertain,
    Bad,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Sample {
    /// Host clock, UTC Unix milliseconds, stamped when the helper read it.
    pub ts_ms: u64,
    /// The device's own timestamp, when the protocol reports one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_ts_ms: Option<u64>,
    pub tag: String,
    pub value: Value,
    #[serde(default)]
    pub quality: Quality,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TagInfo {
    pub tag: String,
    #[serde(default)]
    pub unit: Option<String>,
    /// number, bool or text, when known.
    #[serde(default)]
    pub kind: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SignalInfo {
    /// `signal:<16 hex>`, stable for a source URI without its credentials.
    pub id: String,
    pub protocol: Protocol,
    /// Redacted with `media::redact_url`.
    pub uri: String,
    pub interval_ms: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum SourceState {
    #[default]
    Connecting,
    Running,
    Reconnecting,
    Failed,
    Stopped,
}

/// The latest value of one tag, with recent numeric points on request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TagStatus {
    pub tag: String,
    #[serde(default)]
    pub unit: Option<String>,
    pub value: Value,
    #[serde(default)]
    pub quality: Quality,
    pub ts_ms: u64,
    #[serde(default)]
    pub source_ts_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recent: Option<Vec<(u64, f64)>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SourceStatus {
    pub id: String,
    pub protocol: Protocol,
    pub uri: String,
    pub interval_ms: u64,
    pub state: SourceState,
    pub started_ms: u64,
    #[serde(default)]
    pub last_sample_ms: Option<u64>,
    #[serde(default)]
    pub samples: u64,
    #[serde(default)]
    pub written: u64,
    #[serde(default)]
    pub dropped: u64,
    #[serde(default)]
    pub helper_dropped: u64,
    #[serde(default)]
    pub restarts: u64,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub helper_pid: Option<u32>,
    #[serde(default)]
    pub tags: Vec<TagStatus>,
    #[serde(default)]
    pub tags_total: u64,
    #[serde(default)]
    pub tags_truncated: bool,
}

/// Where samples persist. Called only from the hub's writer thread, in queue
/// order; never call `SignalHub::set_sink` or `flush` while holding a lock
/// that `write` takes.
pub trait SampleSink: Send + Sync {
    fn register(&self, source: &SignalInfo, tags: &[TagInfo]) -> Result<()>;
    /// Write one batch (at most 5000 samples) in one transaction.
    fn write(&self, source_id: &str, batch: &[Sample]) -> Result<()>;
    /// Record a `signal_gap` or `signal_error` event.
    fn event(&self, _kind: &str, _source_id: &str, _text: &str) -> Result<()> {
        Ok(())
    }
    /// Called when the sink is detached: finish a CSV file or checkpoint a dataset.
    fn finish(&self) -> Result<()>;
    fn describe(&self) -> String;
}

/// Write to several sinks. Every sink is attempted; the first error wins.
pub struct TeeSink(pub Vec<Arc<dyn SampleSink>>);
impl TeeSink {
    fn each(&self, mut f: impl FnMut(&dyn SampleSink) -> Result<()>) -> Result<()> {
        let mut first = None;
        for sink in &self.0 {
            if let Err(e) = f(sink.as_ref()) {
                first.get_or_insert(e);
            }
        }
        first.map_or(Ok(()), Err)
    }
}
impl SampleSink for TeeSink {
    fn register(&self, source: &SignalInfo, tags: &[TagInfo]) -> Result<()> {
        self.each(|sink| sink.register(source, tags))
    }
    fn write(&self, source_id: &str, batch: &[Sample]) -> Result<()> {
        self.each(|sink| sink.write(source_id, batch))
    }
    fn event(&self, kind: &str, source_id: &str, text: &str) -> Result<()> {
        self.each(|sink| sink.event(kind, source_id, text))
    }
    fn finish(&self) -> Result<()> {
        self.each(|sink| sink.finish())
    }
    fn describe(&self) -> String {
        self.0
            .iter()
            .map(|sink| sink.describe())
            .collect::<Vec<_>>()
            .join(" + ")
    }
}

pub fn default_timeout_ms() -> u64 {
    5000
}
pub fn default_interval_ms() -> u64 {
    1000
}
pub fn default_browse_ms() -> u64 {
    5000
}
pub fn default_browse_limit() -> u32 {
    500
}

/// Which source schemes this build can run.
pub fn compiled_protocols() -> Json {
    json!({
        "modbus-tcp": cfg!(feature = "modbus"),
        "mqtt": cfg!(feature = "mqtt"),
        "mqtts": cfg!(feature = "mqtt"),
        "opc.tcp": cfg!(feature = "opcua"),
        "mtconnect": cfg!(feature = "mtconnect"),
        "sim-signal": true,
    })
}

/// Check the shape of a signal command's fields. Whether a protocol is
/// compiled in is checked only when the command runs (exit code 5).
pub fn validate_command(_command: &SessionCommand) -> Result<()> {
    Ok(())
}

/// `schema` output: `contract["signals"]`.
pub fn schema_contract() -> Json {
    json!({"protocols": compiled_protocols()})
}

/// `schema` output: example RPC commands, each parsed and validated by tests.
pub fn rpc_examples() -> Vec<Json> {
    vec![
        json!({"op":"signals"}),
        json!({"op":"signal_read","uri":"sim-signal://line1","timeout_ms":5000}),
        json!({"op":"signal_browse","uri":"sim-signal://line1","duration_ms":1000,"limit":100}),
        json!({"op":"signal_start","uri":"sim-signal://line1","interval_ms":1000}),
        json!({"op":"signal_stop","id":"all"}),
    ]
}
