//! Sensor and PLC signals: Modbus TCP, MQTT, OPC UA, MTConnect and a
//! simulator. A separate subsystem from cameras: each source runs in its own
//! `__signal` helper process and the coordinator's `SignalHub` collects its
//! samples into a bounded in-memory view and an optional `SampleSink`.
pub mod auth;
pub mod csv;
pub mod helper;
pub mod hub;
pub mod log;
#[cfg(feature = "modbus")]
pub mod modbus;
#[cfg(feature = "mqtt")]
pub mod mqtt;
#[cfg(feature = "mtconnect")]
pub mod mtconnect;
#[cfg(feature = "opcua")]
pub mod opcua;
pub mod process;
pub mod report;
pub mod sim;
pub mod uri;

pub use uri::{SignalUri, Target, parse};

use crate::session::SessionCommand;
use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use std::{path::PathBuf, sync::Arc};

/// Most sources one session logs at once.
pub const MAX_SOURCES: usize = 16;
/// Most distinct tags one source keeps.
pub const MAX_TAGS: usize = 4096;
/// Most entries in a `--tag` allow-list.
pub const MAX_ALLOW: usize = 1024;
/// Longest tag name, in bytes.
pub const MAX_TAG: usize = 256;
/// Longest text value, in bytes.
pub const MAX_TEXT: usize = 1024;
/// Largest MQTT payload decoded.
pub const MAX_PAYLOAD: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Protocol {
    ModbusTcp,
    Mqtt,
    Opcua,
    Mtconnect,
    SimSignal,
}

impl Protocol {
    /// The Cargo feature that compiles this protocol in.
    pub fn feature(self) -> &'static str {
        match self {
            Self::ModbusTcp => "modbus",
            Self::Mqtt => "mqtt",
            Self::Opcua => "opcua",
            Self::Mtconnect => "mtconnect",
            Self::SimSignal => "",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::ModbusTcp => "Modbus TCP",
            Self::Mqtt => "MQTT",
            Self::Opcua => "OPC UA",
            Self::Mtconnect => "MTConnect",
            Self::SimSignal => "Simulated",
        }
    }
    fn compiled(self) -> bool {
        match self {
            Self::ModbusTcp => cfg!(feature = "modbus"),
            Self::Mqtt => cfg!(feature = "mqtt"),
            Self::Opcua => cfg!(feature = "opcua"),
            Self::Mtconnect => cfg!(feature = "mtconnect"),
            Self::SimSignal => true,
        }
    }
}

/// Fail (exit 5) when this build lacks the protocol.
pub fn require_compiled(protocol: Protocol) -> Result<()> {
    if !protocol.compiled() {
        bail!(
            "{} signal sources are not supported in this build (rebuild with --features {})",
            protocol.label(),
            protocol.feature()
        );
    }
    Ok(())
}

/// What a helper does with its source.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Mode {
    /// One round of values, then exit.
    Read,
    /// List nodes, topics or data items for a bounded time.
    Browse { duration_ms: u64, limit: u32 },
    /// Poll or subscribe until stopped.
    Log,
}

/// What a `__signal` helper runs; the first line of its stdin. The URI may
/// carry credentials, so this never travels in argv and `Debug` redacts it.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignalSpec {
    pub uri: String,
    /// Resolved only for helper stdin; never part of a session command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credentials: Option<auth::SignalCredentials>,
    pub mode: Mode,
    pub interval_ms: u64,
    #[serde(default)]
    pub tags: Vec<String>,
    pub timeout_ms: u64,
    /// OPC UA's certificate store: `<runtime dir>/signals/opcua-pki`.
    #[serde(default)]
    pub pki_dir: Option<PathBuf>,
}
impl std::fmt::Debug for SignalSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SignalSpec")
            .field("uri", &crate::media::redact_url(&self.uri))
            .field("mode", &self.mode)
            .field("interval_ms", &self.interval_ms)
            .field("tags", &self.tags.len())
            .field("timeout_ms", &self.timeout_ms)
            .finish()
    }
}

/// `signal:<16 hex>`: FNV-1a of the redacted URI, so the ID never depends on
/// credentials (the same fold as media source IDs).
pub fn source_id(uri: &str) -> String {
    let hash = crate::media::redact_url(uri)
        .bytes()
        .fold(0xcbf29ce484222325u64, |h, b| {
            (h ^ b as u64).wrapping_mul(0x100000001b3)
        });
    format!("signal:{hash:016x}")
}

/// Redact a message that may quote the source URI or its password.
pub fn redact_text(text: &str, uri: &str, password: Option<&str>) -> String {
    let mut text = text.replace(uri, &crate::media::redact_url(uri));
    if let Some(password) = password.filter(|p| !p.is_empty()) {
        text = text.replace(password, "<redacted>");
    }
    text
}

/// A finite number as a good value; NaN and infinities as bad text, since
/// JSON cannot carry them.
pub fn sanitize_number(x: f64) -> (Value, Quality) {
    if x.is_finite() {
        (Value::Number(x), Quality::Good)
    } else if x.is_nan() {
        (Value::Text("NaN".into()), Quality::Bad)
    } else if x > 0.0 {
        (Value::Text("inf".into()), Quality::Bad)
    } else {
        (Value::Text("-inf".into()), Quality::Bad)
    }
}

/// Cut text to at most `max` bytes on a character boundary, marking the cut.
pub fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max.saturating_sub('…'.len_utf8());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// One reading. Numbers are always finite; text is at most 1 KiB.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum Value {
    Bool(bool),
    Number(f64),
    Text(String),
}

impl Value {
    /// A text value, truncated to 1 KiB.
    pub fn text(text: &str) -> Self {
        Self::Text(truncate(text, MAX_TEXT))
    }
    /// The value as a number for sparklines: booleans are 0 and 1.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Number(n) => Some(*n),
            Self::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            Self::Text(_) => None,
        }
    }
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Bool(_) => "bool",
            Self::Number(_) => "number",
            Self::Text(_) => "text",
        }
    }
}
impl std::fmt::Display for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bool(b) => write!(f, "{b}"),
            Self::Number(n) => write!(f, "{n}"),
            Self::Text(t) => f.write_str(t),
        }
    }
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

/// Independent coordinator limits for one capture instance. Bytes count the
/// UTF-8 JSON encoding of each accepted `Sample`, excluding batch framing.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CaptureLimits {
    pub duration_ms: u64,
    pub max_samples: u64,
    pub max_bytes: u64,
}

impl CaptureLimits {
    pub fn validate(self) -> Result<()> {
        ensure!(
            (1..=86_400_000).contains(&self.duration_ms),
            "signal capture duration must be 1ms..24h"
        );
        ensure!(
            (1..=100_000_000).contains(&self.max_samples),
            "signal capture samples must be 1..100000000"
        );
        ensure!(
            (1..=1_099_511_627_776).contains(&self.max_bytes),
            "signal capture bytes must be 1..1TiB"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CaptureCompletion {
    Duration,
    MaxSamples,
    MaxBytes,
    Stopped,
    Failed,
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

/// Cached coordinator persistence readiness; never opens or contacts a source.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SignalSinkStatus {
    #[serde(default)]
    pub attached: bool,
    /// Changes for every sink attachment/detachment, including the same Slot.
    #[serde(default)]
    pub generation: u64,
    /// Read dynamically from the attached sink, including a lazily opened Slot.
    #[serde(default)]
    pub dataset_slot_id: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub dropped_total: u64,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<auth::AuthBinding>,
    /// Opaque identity of this instance, retained through helper restarts.
    #[serde(default)]
    pub capture_id: String,
    /// The dataset instance this bounded capture is allowed to persist into.
    #[serde(default)]
    pub dataset_slot_id: Option<String>,
    pub protocol: Protocol,
    pub uri: String,
    pub interval_ms: u64,
    pub state: SourceState,
    pub started_ms: u64,
    #[serde(default)]
    pub capture_limits: Option<CaptureLimits>,
    #[serde(default)]
    pub deadline_ms: Option<u64>,
    /// Present only after admission closes and the final flush is acknowledged
    /// or its failure/timeout has been recorded.
    #[serde(default)]
    pub completed_ms: Option<u64>,
    #[serde(default)]
    pub completion: Option<CaptureCompletion>,
    #[serde(default)]
    pub payload_bytes: u64,
    #[serde(default)]
    pub written_bytes: u64,
    #[serde(default)]
    pub persistence_complete: bool,
    #[serde(default)]
    pub persistence_error: Option<String>,
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

impl Sample {
    /// A sample stamped now on the host clock, with the tag cut to 256 bytes.
    pub fn now(tag: &str, value: Value, quality: Quality) -> Self {
        Self {
            ts_ms: crate::scheduling::now_ms(),
            source_ts_ms: None,
            tag: truncate(tag, MAX_TAG),
            value,
            quality,
        }
    }
    /// A numeric sample; non-finite numbers become bad text.
    pub fn number(tag: &str, x: f64) -> Self {
        let (value, quality) = sanitize_number(x);
        Self::now(tag, value, quality)
    }
}

/// One browse result: an OPC UA node, MQTT topic or MTConnect data item.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BrowseItem {
    pub id: String,
    pub name: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
}

/// One line a helper writes to stdout.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Line {
    /// Connected; repeats with only new tags as they are discovered.
    Ready {
        tags: Vec<TagInfo>,
    },
    Samples {
        samples: Vec<Sample>,
    },
    /// `connected` or `reconnecting`, with the redacted error.
    State {
        state: String,
        #[serde(default)]
        error: Option<String>,
    },
    /// Samples the helper dropped since its last report.
    Dropped {
        count: u64,
    },
    Browse {
        items: Vec<BrowseItem>,
    },
    Done {
        #[serde(default)]
        truncated: bool,
    },
    Error {
        message: String,
    },
}

/// Where samples persist. Called only from the hub's writer thread, in queue
/// order; never call `SignalHub::set_sink` or `flush` while holding a lock
/// that `write` takes.
pub trait SampleSink: Send + Sync {
    /// The exact open dataset instance this sink writes to. Other sink types
    /// cannot satisfy a dataset-scoped capture merely by being attached.
    fn dataset_slot_id(&self) -> Option<String> {
        None
    }
    /// Dataset sinks override these checked methods to compare identity and
    /// write under the same dataset lock.
    fn register_for_dataset_slot(
        &self,
        slot_id: &str,
        source: &SignalInfo,
        tags: &[TagInfo],
    ) -> Result<()> {
        ensure!(
            self.dataset_slot_id().as_deref() == Some(slot_id),
            "signal capture dataset instance changed"
        );
        self.register(source, tags)
    }
    fn write_for_dataset_slot(
        &self,
        slot_id: &str,
        source_id: &str,
        batch: &[Sample],
    ) -> Result<()> {
        ensure!(
            self.dataset_slot_id().as_deref() == Some(slot_id),
            "signal capture dataset instance changed"
        );
        self.write(source_id, batch)
    }
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

/// Check a polling interval: 100 ms to 1 hour.
pub fn validate_interval(interval_ms: u64) -> Result<()> {
    ensure!(
        (100..=3_600_000).contains(&interval_ms),
        "signal interval must be 100ms..1h"
    );
    Ok(())
}

/// Check a `--tag` allow-list.
pub fn validate_tags(tags: &[String]) -> Result<()> {
    ensure!(
        tags.len() <= MAX_ALLOW,
        "at most {MAX_ALLOW} --tag filters are accepted"
    );
    for tag in tags {
        ensure!(
            !tag.is_empty() && tag.len() <= MAX_TAG && !tag.chars().any(char::is_control),
            "each --tag must be 1..{MAX_TAG} bytes without control characters"
        );
    }
    Ok(())
}

/// Check a `signal_stop` ID: `all` or `signal:<16 lowercase hex>`.
pub fn validate_id(id: &str) -> Result<()> {
    ensure!(
        id == "all"
            || id.strip_prefix("signal:").is_some_and(|hex| {
                hex.len() == 16 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
            }),
        "signal source ID must be 'all' or signal:<16 lowercase hex digits> (see `capturefab signals`)"
    );
    Ok(())
}

pub fn validate_capture_id(id: &str) -> Result<()> {
    ensure!(
        crate::session::valid_forwarding_id(id),
        "signal capture ID must be 32 lowercase hex digits (see `capturefab signals`)"
    );
    Ok(())
}

/// Parse a URI that reads values: an OPC UA source needs nodes to read
/// (only browse goes without).
pub fn parse_readable(uri: &str) -> Result<SignalUri> {
    let parsed = parse(uri)?;
    if let uri::Target::Opcua { nodes, .. } = &parsed.target {
        ensure!(
            !nodes.is_empty(),
            "opc.tcp:// read, start and log need at least one ?node=NODE_ID; find node IDs with signals browse"
        );
    }
    Ok(parsed)
}

/// Check the shape of a signal command's fields. Whether a protocol is
/// compiled in is checked only when the command runs (exit code 5).
pub fn validate_command(command: &SessionCommand) -> Result<()> {
    let binding = match command {
        SessionCommand::SignalRead { uri, auth, .. }
        | SessionCommand::SignalBrowse { uri, auth, .. }
        | SessionCommand::SignalStart { uri, auth, .. }
        | SessionCommand::SignalCapture { uri, auth, .. } => Some((uri, auth)),
        _ => None,
    };
    if let Some((uri, Some(binding))) = binding {
        binding.validate()?;
        auth::validate_endpoint(uri)?;
    }
    match command {
        SessionCommand::SignalBind { profile } => profile.validate()?,
        SessionCommand::SignalRead {
            uri, timeout_ms, ..
        } => {
            parse_readable(uri)?;
            crate::session::validate_timeout(*timeout_ms)?;
        }
        SessionCommand::SignalBrowse {
            uri,
            duration_ms,
            limit,
            ..
        } => {
            parse(uri)?;
            ensure!(
                (100..=60_000).contains(duration_ms),
                "browse duration must be 100ms..60s"
            );
            ensure!(
                (1..=5000).contains(limit),
                "browse limit must be 1..5000 items"
            );
        }
        SessionCommand::SignalStart {
            uri,
            interval_ms,
            tags,
            ..
        } => {
            parse_readable(uri)?;
            validate_interval(*interval_ms)?;
            validate_tags(tags)?;
        }
        SessionCommand::SignalCapture {
            uri,
            interval_ms,
            tags,
            duration_ms,
            max_samples,
            max_bytes,
            slot_id,
            ..
        } => {
            parse_readable(uri)?;
            validate_interval(*interval_ms)?;
            validate_tags(tags)?;
            CaptureLimits {
                duration_ms: *duration_ms,
                max_samples: *max_samples,
                max_bytes: *max_bytes,
            }
            .validate()?;
            ensure!(
                crate::session::valid_forwarding_id(slot_id),
                "dataset slot ID must be 32 lowercase hex digits"
            );
        }
        SessionCommand::SignalStop { id } => validate_id(id)?,
        SessionCommand::SignalStopIf { id, capture_id } => {
            validate_id(id)?;
            ensure!(
                id != "all",
                "conditional signal stop requires one source ID"
            );
            validate_capture_id(capture_id)?;
        }
        _ => {}
    }
    Ok(())
}

/// `schema` output: `contract["signals"]`.
pub fn schema_contract() -> Json {
    json!({
        "protocols": compiled_protocols(),
        "profiles": {
            "commands": "signals profiles list|save|remove|bind; list/save/remove are local setup, bind requires the selected running workbench",
            "credentials": "save accepts a credential-free endpoint and bounded credentials-stdin JSON; passwords use the OS credential store or a CAPTUREFAB_SIGNAL_ environment reference; usernames, passwords and environment names are omitted from public profile metadata",
            "binding": "signal_bind explicitly resolves one exact profile id/revision and returns an opaque auth epoch plus source reference; it does not contact the endpoint. signal_read, signal_browse, signal_start and signal_capture accept optional auth from that receipt. Old epochs cannot adopt profile or password changes or another coordinator.",
            "builtin_assistant": "assistant run --source-profile NAME --allow-data binds locally before model setup; its provider receives source references, never credential epochs or values"
        },
        "uris": {
            "modbus-tcp": "modbus-tcp://HOST[:502]/UNIT_ID/coil|discrete|input|holding/ADDRESS[?count=N&type=u16|i16|u32|i32|f32|f64|bool&order=abcd|cdab|badc|dcba&scale=F&offset=F&name=TAG&unit=UNIT]; ADDRESS is the 0-based protocol address (Modicon 40001 is holding/0); value = raw*scale+offset; at most 125 registers or 2000 bits per source",
            "mqtt": "mqtt[s]://[USER[:PASS]@]BROKER[:1883|8883]/TOPIC_FILTER[?client_id=ID&qos=0|1]; '#' only as the last level, '+' as a whole level; numeric, true/false, JSON object (flattened to topic/field, depth 4, 256 leaves) or text payloads; binary payloads (not UTF-8, or with control characters other than tab, CR and LF) become bounded 'hex:...' text; a top-level ts, timestamp or time field (Unix s or ms, or RFC 3339 text) becomes source_ts_ms and other such fields stay tags",
            "opc.tcp": "opc.tcp://[USER[:PASS]@]HOST[:4840][/PATH]?node=NODE_ID[&node=...][&root=NODE_ID][&security=none]; node IDs like ns=2;s=Line1.Temp; 1..1024 nodes for read and start; browse starts at root (default i=85, depth 4); SecurityPolicy None only, so passwords travel unencrypted",
            "mtconnect": "mtconnect[s]://HOST[:5000|443][/DEVICE]; polls /current; tags are DEVICE/DATA_ITEM_ID; UNAVAILABLE is quality bad",
            "sim-signal": "sim-signal://LINE[?tags=1..64]; deterministic LINE/temp_c, pressure_bar, count, running, state, vibration",
            "encoding": "percent-encode '@' (%40) in topics and node IDs and '/', '?', '#' and '@' in credentials; credentials are redacted in every output"
        },
        "limits": {
            "sources": MAX_SOURCES,
            "tags_per_source": MAX_TAGS,
            "tag_filters": MAX_ALLOW,
            "interval_ms": [100, 3_600_000],
            "capture_duration_ms": [1, 86_400_000],
            "capture_samples": [1, 100_000_000],
            "capture_bytes": [1, 1_099_511_627_776u64],
            "browse_ms": [100, 60_000],
            "browse_items": 5000,
            "mqtt_payload_bytes": MAX_PAYLOAD,
            "text_bytes": MAX_TEXT,
            "tag_bytes": MAX_TAG,
            "csv_bytes": crate::media::default_recording_cap(),
            "status_tags": {"signals": 256, "snapshot": 32},
            "recent_points": hub::RECENT
        },
        "time": "ts_ms is the host clock (UTC Unix ms) when the helper read the value; source_ts_ms is the device's own timestamp when the protocol has one",
        "drops": "samples dropped by a full helper queue (helper_dropped), the hub queue or sink errors (dropped) are counted; without an attached dataset or CSV sink samples are kept only as latest values and persisting is false",
        "lifetime": "legacy signal_start sources run while the GUI, serve or log process runs and restart up to 5 times after a helper exits; signal_capture requires the exact dataset slot_id and a sink, independently enforces duration/sample/payload-byte bounds and flushes before completed_ms is set; sources are not persisted across sessions",
        "capture_identity": "capture_id is an opaque 32 lowercase hex instance token retained across helper restarts; a replacement of the same URI gets a new token; signal_stop_if refuses a mismatched replacement and never accepts all",
        "sink_binding": "signal_capture and dataset_close_if can pin sink_generation from the dataset_open receipt or current signal_sink status; stale generations fail before acquisition or detachment, including a reattachment of the same dataset Slot",
        "capture_bytes": "payload_bytes and written_bytes sum the UTF-8 JSON encoding of each accepted or successfully written Sample, excluding batch framing; these are payload budgets, not file sizes",
        "sample": {"ts_ms":"u64","source_ts_ms":"u64|null","tag":"string","value":"number|bool|string","quality":"good|uncertain|bad"},
        "results": {
            "signal_bind": {"endpoint":"credential-free local endpoint","reference":"signal:<16 hex>","auth":{"profile":{"id":"32 lowercase hex","revision":"32 lowercase hex"},"epoch":"32 lowercase hex"}},
            "signal_read": {"id":"signal:<16 hex>","uri":"redacted","protocol":"string","tags":"[TagInfo]","samples":"[Sample]","dropped":"u64"},
            "signal_browse": {"id":"string","uri":"redacted","protocol":"string","items":"[{id,name,kind,data_type?,unit?,value?}]","truncated":"bool"},
            "signal_start": "SourceStatus",
            "signal_capture": "SourceStatus (requires slot_id and duration_ms/max_samples/max_bytes; terminal status remains observable until replacement or stop)",
            "signal_stop": {"stopped":"[SourceStatus]"},
            "signal_stop_if": {"stopped":"[SourceStatus]"},
            "signals": {"sources":"[SourceStatus]","persisting":"bool","dropped":"u64","sink_error":"string|null","sink":"SignalSinkStatus"},
            "log": {"sources":"[SourceStatus]","dataset":"path|null","csv":"path|null","samples":"u64","written":"u64","dropped":"u64","duration_ms":"u64","warnings":"[string], only when nothing was logged"}
        },
        "source_status": "{id,capture_id,dataset_slot_id,protocol,uri,interval_ms,state:connecting|running|reconnecting|failed|stopped,started_ms,capture_limits:{duration_ms,max_samples,max_bytes}|null,deadline_ms,completed_ms,completion:duration|max_samples|max_bytes|stopped|failed|null,payload_bytes,written_bytes,persistence_complete,persistence_error,last_sample_ms,samples,written,dropped,helper_dropped,restarts,error,helper_pid,tags:[{tag,unit,value,quality,ts_ms,source_ts_ms,recent?}],tags_total,tags_truncated}",
        "sink_status": "{attached:bool,generation:u64,dataset_slot_id:string|null,error:string|null,dropped_total:u64}; generation changes on every sink attachment/detachment; attached with no dataset_slot_id can be a closed/lazy dataset or a foreign sink; inspect the current dataset before capturing",
        "errors": {"2":"bad URI or arguments","3":"connection refused or unknown source ID","4":"timed out","5":"protocol not compiled in, or browse on modbus-tcp","6":"storage full while writing CSV"},
    })
}

/// `schema` output: example RPC commands, each parsed and validated by tests.
pub fn rpc_examples() -> Vec<Json> {
    vec![
        json!({"op":"signals"}),
        json!({"op":"signal_bind","profile":{"id":"0123456789abcdef0123456789abcdef","revision":"abcdef0123456789abcdef0123456789"}}),
        json!({"op":"signal_read","uri":"sim-signal://line1","timeout_ms":5000}),
        json!({"op":"signal_browse","uri":"sim-signal://line1","duration_ms":1000,"limit":100}),
        json!({"op":"signal_start","uri":"sim-signal://line1","interval_ms":1000}),
        json!({"op":"signal_capture","uri":"sim-signal://line1","interval_ms":1000,"duration_ms":5000,"max_samples":100,"max_bytes":100000,"slot_id":"0123456789abcdef0123456789abcdef","sink_generation":1}),
        json!({"op":"signal_stop","id":"all"}),
        json!({"op":"signal_stop_if","id":"signal:0123456789abcdef","capture_id":"0123456789abcdef0123456789abcdef"}),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opcua_reads_need_nodes_but_browse_does_not() {
        let bare = "opc.tcp://plc:4840/";
        assert!(parse(bare).is_ok());
        let error = parse_readable(bare).unwrap_err();
        assert!(error.to_string().contains("?node="), "{error}");
        assert!(parse_readable("opc.tcp://plc:4840/?node=i=2258").is_ok());
        assert!(
            validate_command(&SessionCommand::SignalRead {
                uri: bare.into(),
                auth: None,
                timeout_ms: 2000
            })
            .is_err()
        );
    }

    #[test]
    fn ids_and_redaction_ignore_credentials() {
        assert_eq!(source_id("mqtt://u:p@h/t"), source_id("mqtt://x:y@h/t"));
        assert_ne!(source_id("mqtt://h/t"), source_id("mqtt://h/u"));
        assert!(source_id("sim-signal://a").starts_with("signal:"));
        validate_id(&source_id("sim-signal://a")).unwrap();
        assert!(validate_id("signal:ABC").is_err());
        let spec = SignalSpec {
            uri: "mqtt://user:hunter2@h/t".into(),
            credentials: None,
            mode: Mode::Read,
            interval_ms: 1000,
            tags: vec![],
            timeout_ms: 5000,
            pki_dir: None,
        };
        let debug = format!("{spec:?}");
        assert!(!debug.contains("hunter2") && debug.contains("<redacted>"));
        assert_eq!(
            redact_text(
                "failed: hunter2 mqtt://user:hunter2@h/t",
                &spec.uri,
                Some("hunter2")
            ),
            "failed: <redacted> mqtt://<redacted>@h/t"
        );
    }

    #[test]
    fn non_finite_numbers_become_bad_text() {
        for (x, text) in [
            (f64::NAN, "NaN"),
            (f64::INFINITY, "inf"),
            (f64::NEG_INFINITY, "-inf"),
        ] {
            let sample = Sample::number("t", x);
            assert_eq!(sample.value, Value::Text(text.into()));
            assert_eq!(sample.quality, Quality::Bad);
            let back: Sample =
                serde_json::from_str(&serde_json::to_string(&sample).unwrap()).unwrap();
            assert_eq!(back, sample);
        }
        assert!(truncate(&"é".repeat(600), MAX_TEXT).len() <= MAX_TEXT);
    }

    #[test]
    fn validates_commands() {
        let ok = |json: &str| {
            let command: SessionCommand = serde_json::from_str(json).unwrap();
            command.validate()
        };
        ok(r#"{"op":"signal_start","uri":"sim-signal://a","interval_ms":100,"tags":["a/temp_c"]}"#)
            .unwrap();
        assert!(ok(r#"{"op":"signal_start","uri":"sim-signal://a","interval_ms":99}"#).is_err());
        assert!(
            ok(r#"{"op":"signal_browse","uri":"sim-signal://a","duration_ms":60001}"#).is_err()
        );
        assert!(ok(r#"{"op":"signal_browse","uri":"sim-signal://a","limit":0}"#).is_err());
        assert!(ok(r#"{"op":"signal_read","uri":"sim-signal://a","timeout_ms":0}"#).is_err());
        assert!(ok(r#"{"op":"signal_start","uri":"sim-signal://a","tags":[""]}"#).is_err());
        assert!(ok(r#"{"op":"signal_stop","id":"x"}"#).is_err());
        assert!(SessionCommand::from_json_slice(br#"{"op":"signals","x":1}"#).is_err());
    }

    #[test]
    fn bounded_commands_require_finite_limits_and_exact_identities() {
        let base = json!({"op":"signal_capture", "uri":"sim-signal://a", "duration_ms":1000, "max_samples":100, "max_bytes":10000, "slot_id":"0123456789abcdef0123456789abcdef"});
        let parse = |value: &Json| -> Result<SessionCommand> {
            let command = SessionCommand::from_json_slice(&serde_json::to_vec(value).unwrap())?;
            command.validate()?;
            Ok(command)
        };
        let command = parse(&base).unwrap();
        assert!(command.is_signal());
        for (field, value) in [
            ("duration_ms", 0),
            ("duration_ms", 86_400_001),
            ("max_samples", 0),
            ("max_samples", 100_000_001),
            ("max_bytes", 0),
            ("max_bytes", 1_099_511_627_777u64),
        ] {
            let mut invalid = base.clone();
            invalid[field] = json!(value);
            assert!(parse(&invalid).is_err(), "{field}: {value}");
        }
        let mut invalid = base.clone();
        invalid["slot_id"] = json!("all");
        assert!(parse(&invalid).is_err());
        let stop = json!({"op":"signal_stop_if", "id":"signal:0123456789abcdef", "capture_id":"0123456789abcdef0123456789abcdef"});
        assert!(parse(&stop).unwrap().is_signal());
        for (field, value) in [
            ("id", "all"),
            ("capture_id", ""),
            ("capture_id", "0123456789ABCDEF0123456789ABCDEF"),
        ] {
            let mut invalid = stop.clone();
            invalid[field] = json!(value);
            assert!(parse(&invalid).is_err());
        }
    }

    #[test]
    fn missing_features_are_unsupported() {
        for protocol in [
            Protocol::ModbusTcp,
            Protocol::Mqtt,
            Protocol::Opcua,
            Protocol::Mtconnect,
        ] {
            if !protocol.compiled() {
                let error = require_compiled(protocol).unwrap_err();
                assert_eq!(crate::cli::error_code(&error).1, 5);
            }
        }
        let error = anyhow::anyhow!(
            "MQTT signal sources are not supported in this build (rebuild with --features mqtt)"
        );
        assert_eq!(crate::cli::error_code(&error).1, 5);
        require_compiled(Protocol::SimSignal).unwrap();
    }
}
