//! Checked sensor acquisition and dataset operations shared by every agent.
use super::{
    Checked, FieldKind, FieldSpec, Issue, IssueCode, Op, Origin, PlanState, Risk, ToolCall,
    ToolKind, ToolSpec,
    context::{Context, DataSourceRef, ToolHost},
    parse_args, schema,
};
use crate::{
    dataset::{ExportFormat, OpenMode},
    session::SessionCommand,
};
use anyhow::{Context as _, Result, bail, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

pub const PLANNED_SLOT: &str = "00000000000000000000000000000000";
const QUERY_BYTES: u32 = 16 * 1024;

const fn field(
    key: &'static str,
    label: &'static str,
    kind: FieldKind,
    optional: bool,
    help: &'static str,
) -> FieldSpec {
    FieldSpec {
        key,
        label,
        kind,
        optional,
        help,
        ui_only: false,
    }
}
const SOURCE: FieldSpec = field(
    "source",
    "Data source",
    FieldKind::Text,
    false,
    "An approved signal reference from list_data_sources; a CLI caller may supply a source URI.",
);
const TIMEOUT: FieldSpec = field(
    "timeout",
    "At most",
    FieldKind::Duration,
    false,
    "A finite duration such as 5s, 30s or 2m.",
);
const fn bytes(label: &'static str, help: &'static str) -> FieldSpec {
    field(
        "max_bytes",
        label,
        FieldKind::Integer {
            min: 1,
            max: 1_099_511_627_776,
        },
        false,
        help,
    )
}
const MODES: &[(&str, &str)] = &[
    ("auto", "Open or create"),
    ("create", "Create new"),
    ("open", "Open existing"),
];
const FORMATS: &[(&str, &str)] = &[
    ("csv", "CSV"),
    ("parquet", "Parquet"),
    ("json", "JSON"),
    ("jsonl", "JSON Lines"),
];
const SIGNAL_FIELDS: &[FieldSpec] = &[
    SOURCE,
    field(
        "every",
        "Sample interval",
        FieldKind::Duration,
        false,
        "Sampling interval, at least 100ms.",
    ),
    field(
        "duration",
        "Stop after",
        FieldKind::Duration,
        false,
        "Logging stops on the server even if the agent disconnects.",
    ),
    field(
        "max_samples",
        "Maximum samples",
        FieldKind::Integer {
            min: 1,
            max: 100_000_000,
        },
        false,
        "Individual tag values, not polling rounds.",
    ),
    bytes(
        "Payload bytes",
        "Serialized sample payload ceiling; physical database and WAL bytes are separate.",
    ),
    field(
        "tags",
        "Tags",
        FieldKind::StringList,
        true,
        "Only these tag names, or empty for all advertised tags.",
    ),
];
const STOP_FIELDS: &[FieldSpec] = &[SOURCE];
const WAIT_FIELDS: &[FieldSpec] = &[
    SOURCE,
    TIMEOUT,
    field(
        "samples",
        "Persisted samples",
        FieldKind::Integer {
            min: 1,
            max: 100_000_000,
        },
        true,
        "Wait for at least this many persisted samples, or empty for completed capture and final flush.",
    ),
];
const BROWSE_FIELDS: &[FieldSpec] = &[
    SOURCE,
    TIMEOUT,
    field(
        "limit",
        "Maximum items",
        FieldKind::Integer { min: 1, max: 1000 },
        true,
        "At most 1,000 discovered items; 100 by default.",
    ),
];
const READ_FIELDS: &[FieldSpec] = &[SOURCE, TIMEOUT];
const OPEN_FIELDS: &[FieldSpec] = &[
    field(
        "file_name",
        "Dataset file",
        FieldKind::FileName,
        false,
        "A .duckdb file inside the capture folder.",
    ),
    field(
        "mode",
        "Open mode",
        FieldKind::Choice(MODES),
        false,
        "Create refuses an existing file; open requires one.",
    ),
    bytes(
        "Dataset size policy",
        "Dataset-wide size policy, checked during writes; allocations may temporarily exceed it. Minimum 16 MiB.",
    ),
];
const QUERY_FIELDS: &[FieldSpec] = &[
    field(
        "sql",
        "Read-only query",
        FieldKind::Text,
        false,
        "One SELECT statement; never reads files outside this dataset.",
    ),
    field(
        "limit",
        "Maximum rows",
        FieldKind::Integer { min: 1, max: 1000 },
        true,
        "At most 1,000 rows; 100 by default, with a separate 16 KiB response limit.",
    ),
    TIMEOUT,
];
const NOTE_FIELDS: &[FieldSpec] = &[field(
    "text",
    "Note",
    FieldKind::Text,
    false,
    "A timestamped note saved in the dataset.",
)];
const EXPORT_FIELDS: &[FieldSpec] = &[
    field(
        "folder",
        "New export folder",
        FieldKind::FileName,
        false,
        "A new folder inside the capture folder; existing files are never replaced.",
    ),
    field(
        "format",
        "File format",
        FieldKind::Choice(FORMATS),
        false,
        "Export format.",
    ),
    field(
        "tables",
        "Tables",
        FieldKind::StringList,
        true,
        "Selected tables, or empty for the standard analysis bundle.",
    ),
    field(
        "since_ms",
        "From Unix milliseconds",
        FieldKind::Integer {
            min: 0,
            max: i64::MAX,
        },
        true,
        "Optional inclusive start time.",
    ),
    field(
        "until_ms",
        "Until Unix milliseconds",
        FieldKind::Integer {
            min: 0,
            max: i64::MAX,
        },
        true,
        "Optional inclusive end time.",
    ),
    bytes(
        "Export bytes",
        "Hard ceiling on all generated bundle files, including its manifest and loading instructions.",
    ),
    TIMEOUT,
];
const fn spec(
    name: &'static str,
    title: &'static str,
    kind: ToolKind,
    risk: Risk,
    description: &'static str,
    schema: fn() -> Value,
    fields: &'static [FieldSpec],
) -> ToolSpec {
    ToolSpec {
        name,
        title,
        kind,
        risk,
        needs_session: true,
        needs_vision: false,
        description,
        schema,
        fields,
    }
}
pub const LIST_DATA_SOURCES: ToolSpec = spec(
    "list_data_sources",
    "List data sources",
    ToolKind::Read,
    Risk::Observe,
    "List approved source references and cached source status, counts, limits and errors. With data permission, include bounded latest values, units, quality and timestamps. Never connects or starts logging. Use opaque source references in other tools; never invent an endpoint.",
    schema::no_arguments,
    &[],
);
pub const GET_DATASET: ToolSpec = spec(
    "get_dataset",
    "Inspect dataset",
    ToolKind::Read,
    Risk::Observe,
    "Inspect the current dataset identity, row counts, size, limits and errors, plus the standard table names. Does not query rows or open a file. Use query_dataset for read-only SQL and export_dataset for an analysis bundle.",
    schema::no_arguments,
    &[],
);
pub const BROWSE_DATA_SOURCE: ToolSpec = spec(
    "browse_data_source",
    "Browse data source",
    ToolKind::Control,
    Risk::Bounded,
    "Contact one human-approved source for a bounded browse. timeout is 1s..30s and limit is 1..1000 (100 by default). New network connections require disruptive-action approval. Values and tag metadata may be sent to the model only with data permission. Never starts persistent logging.",
    browse_schema,
    BROWSE_FIELDS,
);
pub const READ_DATA_SOURCE: ToolSpec = spec(
    "read_data_source",
    "Read data source",
    ToolKind::Control,
    Risk::Bounded,
    "Contact one human-approved source for a single bounded read, timeout 1s..30s. New network connections require disruptive-action approval. Values may be sent to the model only with data permission. This is a measurement, not persisted data; use start_signal_capture to save samples.",
    read_schema,
    READ_FIELDS,
);
pub const OPEN_DATASET: ToolSpec = spec(
    "open_dataset",
    "Open dataset",
    ToolKind::Configure,
    Risk::Bounded,
    "Open or create a .duckdb catalog inside the capture folder, with a positive dataset-wide size policy checked during writes; physical allocations can overshoot. The checked current slot is matched again before opening. An already-open file keeps its size policy; close it with approval before changing that policy or opening another file. Never overwrites or clears a database.",
    open_schema,
    OPEN_FIELDS,
);
pub const CLOSE_DATASET: ToolSpec = spec(
    "close_dataset",
    "Close dataset",
    ToolKind::Control,
    Risk::Disruptive,
    "Close only the exact reviewed dataset instance. This detaches the common sample sink and may affect other work, so approval is required. The database and saved captures remain on disk.",
    schema::no_arguments,
    &[],
);
pub const START_SIGNAL_CAPTURE: ToolSpec = spec(
    "start_signal_capture",
    "Capture sensor data",
    ToolKind::Capture,
    Risk::Bounded,
    "Log an approved source into the exact current dataset. Requires explicit every, duration, max_samples and max_bytes; stops at the first limit even after the agent disconnects. Counts individual tag samples and serialized sample payload bytes. Returns a capture identity; use wait_for_data to verify persistence. Refuses an already active source.",
    signal_schema,
    SIGNAL_FIELDS,
);
pub const STOP_SIGNAL_CAPTURE: ToolSpec = spec(
    "stop_signal_capture",
    "Stop sensor capture",
    ToolKind::Control,
    Risk::Disruptive,
    "Stop and flush only the exact reviewed source capture instance. Never stops all sources or a replacement started by a person. Stopping the task's own capture is bounded; stopping someone else's needs approval.",
    source_schema,
    STOP_FIELDS,
);
pub const WAIT_FOR_DATA: ToolSpec = spec(
    "wait_for_data",
    "Wait for sensor data",
    ToolKind::Control,
    Risk::Observe,
    "Wait at most timeout (1s..10m) for the reviewed capture. samples null waits for terminal completion and final persistence flush; a number waits for that many written samples. Reports failure, loss, replacement or timeout rather than claiming persistence. Cancellation stops waiting, not another person's capture.",
    wait_schema,
    WAIT_FIELDS,
);
pub const QUERY_DATASET: ToolSpec = spec(
    "query_dataset",
    "Query dataset",
    ToolKind::Read,
    Risk::Observe,
    "Run one read-only SELECT against the exact current dataset, timeout 1s..30s, limit 1..1000 (100 by default). Built-in model queries first copy the standard tables into a bounded private snapshot, removing camera serials and private locator details before SQL runs. A snapshot beyond 100,000 rows or 16 MiB of transferred data fails explicitly. Results have a 16 KiB cap with truncation markers. Requires permission to send data to the model. Use get_dataset for table names; captures_with_signals joins images with their latest signals.",
    query_schema,
    QUERY_FIELDS,
);
pub const ANNOTATE_DATASET: ToolSpec = spec(
    "annotate_dataset",
    "Add dataset note",
    ToolKind::Control,
    Risk::Bounded,
    "Save a timestamped note of at most 4,096 bytes in the exact reviewed dataset. Does not change captured samples, files or camera settings.",
    note_schema,
    NOTE_FIELDS,
);
pub const EXPORT_DATASET: ToolSpec = spec(
    "export_dataset",
    "Export analysis bundle",
    ToolKind::Capture,
    Risk::Bounded,
    "Export the exact reviewed dataset into a new folder under the capture folder, with a hard bundle max_bytes and timeout. Each attempt reserves twice max_bytes for staged and published copies, including failed attempts. Keep max_bytes within limits_remaining.export_bundle_bytes. Select csv, parquet, json or jsonl and optional tables/time range. Returns manifest, row counts, files and checksums. Existing folders are refused. Unsupported bounded formats fail explicitly.",
    export_schema,
    EXPORT_FIELDS,
);

fn source_property() -> Value {
    schema::string(
        "An approved source reference from list_data_sources. CLI callers may supply a complete source URI.",
    )
}
fn source_schema() -> Value {
    schema::object(vec![("source", source_property())])
}
fn read_schema() -> Value {
    schema::object(vec![
        ("source", source_property()),
        ("timeout", schema::string("1s..30s")),
    ])
}
fn browse_schema() -> Value {
    schema::object(vec![
        ("source", source_property()),
        ("timeout", schema::string("1s..30s")),
        (
            "limit",
            schema::nullable(schema::integer("1..1000, default 100")),
        ),
    ])
}
fn signal_schema() -> Value {
    schema::object(vec![
        ("source", source_property()),
        ("every", schema::string("100ms..1h")),
        ("duration", schema::string("1s..24h")),
        (
            "max_samples",
            schema::integer("1..100000000 individual samples"),
        ),
        (
            "max_bytes",
            schema::integer("Positive serialized sample payload byte ceiling"),
        ),
        (
            "tags",
            schema::nullable(schema::array(schema::string("Tag name"), "Tag allowlist")),
        ),
    ])
}
fn wait_schema() -> Value {
    schema::object(vec![
        ("source", source_property()),
        ("timeout", schema::string("1s..10m")),
        (
            "samples",
            schema::nullable(schema::integer(
                "1..100000000 persisted samples, or null for completion and flush",
            )),
        ),
    ])
}
fn open_schema() -> Value {
    schema::object(vec![
        (
            "file_name",
            schema::string("Relative .duckdb file inside capture folder"),
        ),
        (
            "mode",
            schema::choice(&["auto", "create", "open"], "Open mode"),
        ),
        (
            "max_bytes",
            schema::integer(
                "Positive dataset-wide size policy in bytes; allocation can overshoot; minimum 16 MiB",
            ),
        ),
    ])
}
fn query_schema() -> Value {
    schema::object(vec![
        ("sql", schema::string("One read-only SELECT")),
        (
            "limit",
            schema::nullable(schema::integer("1..1000 rows, default 100")),
        ),
        ("timeout", schema::string("1s..30s")),
    ])
}
fn note_schema() -> Value {
    schema::object(vec![(
        "text",
        schema::string("Timestamped note, at most 4096 bytes"),
    )])
}
fn export_schema() -> Value {
    schema::object(vec![
        (
            "folder",
            schema::string("New relative folder inside capture root"),
        ),
        (
            "format",
            schema::choice(&["csv", "parquet", "json", "jsonl"], "Export format"),
        ),
        (
            "tables",
            schema::nullable(schema::array(
                schema::string("Table name"),
                "Selected tables",
            )),
        ),
        (
            "since_ms",
            schema::nullable(schema::integer("Inclusive start time")),
        ),
        (
            "until_ms",
            schema::nullable(schema::integer("Inclusive end time")),
        ),
        (
            "max_bytes",
            schema::integer(
                "Maximum complete bundle bytes; reserves twice this amount for staged and published copies",
            ),
        ),
        ("timeout", schema::string("1s..10m")),
    ])
}

#[derive(Deserialize)]
struct SourceArgs {
    source: String,
}
#[derive(Deserialize)]
struct ReadArgs {
    source: String,
    timeout: String,
}
#[derive(Deserialize)]
struct BrowseArgs {
    source: String,
    timeout: String,
    limit: Option<u32>,
}
#[derive(Deserialize)]
struct SignalArgs {
    source: String,
    every: String,
    duration: String,
    max_samples: u64,
    max_bytes: u64,
    tags: Option<Vec<String>>,
}
#[derive(Deserialize)]
struct WaitArgs {
    source: String,
    timeout: String,
    samples: Option<u64>,
}
#[derive(Deserialize)]
struct OpenArgs {
    file_name: String,
    mode: OpenMode,
    max_bytes: u64,
}
#[derive(Deserialize)]
struct QueryArgs {
    sql: String,
    limit: Option<u32>,
    timeout: String,
}
#[derive(Deserialize)]
struct NoteArgs {
    text: String,
}
#[derive(Deserialize)]
struct ExportArgs {
    folder: String,
    format: ExportFormat,
    tables: Option<Vec<String>>,
    since_ms: Option<u64>,
    until_ms: Option<u64>,
    max_bytes: u64,
    timeout: String,
}

fn duration(field: &str, value: &str, max: u64) -> Result<u64, Issue> {
    let (ms, _) = super::values::duration(field, value, if field == "every" { 100 } else { 1000 })?;
    if ms > max {
        return Err(Issue::usage(
            Some(field),
            format!(
                "Use {field} no longer than {}.",
                crate::scheduling::format_duration(max)
            ),
        ));
    }
    Ok(ms)
}
fn source(ctx: &Context, origin: Origin, value: &str) -> Result<DataSourceRef, Issue> {
    if let Some(source) = ctx
        .data_sources
        .iter()
        .find(|source| source.reference == value && !source.uri.is_empty())
    {
        source
            .validate_in(ctx)
            .map_err(|error| Issue::usage(Some("source"), error.to_string()))?;
        return Ok(source.clone());
    }
    if origin == Origin::Cli {
        if let Some(receipt) = ctx
            .snapshot
            .signal_bindings
            .iter()
            .find(|receipt| receipt.reference == value)
        {
            let source = DataSourceRef::from_binding(receipt)
                .map_err(|error| Issue::usage(Some("source"), error.to_string()))?;
            source
                .validate_in(ctx)
                .map_err(|error| Issue::usage(Some("source"), error.to_string()))?;
            return Ok(source);
        }
        return DataSourceRef::from_uri(value).map_err(|error| {
            Issue::usage(
                Some("source"),
                super::redact::for_output_text(&error.to_string()),
            )
        });
    }
    Err(Issue::usage(
        Some("source"),
        "Choose an approved source from the task's data sources. Add its URI in task setup first.",
    ))
}
fn source_key(ctx: &Context, origin: Origin, value: &str) -> Result<String, Issue> {
    if ctx.snapshot.signals.iter().any(|source| source.id == value)
        || ctx
            .data_sources
            .iter()
            .any(|source| source.reference == value)
    {
        return Ok(value.into());
    }
    source(ctx, origin, value).map(|source| source.reference)
}
fn permit(ctx: &Context, origin: Origin) -> Result<(), Issue> {
    if origin == Origin::Assistant && !ctx.allow_data {
        return Err(Issue::usage(
            None,
            "Allow data access before sending source values, dataset rows or notes to the assistant.",
        ));
    }
    Ok(())
}
fn dataset(plan: &PlanState, checked: &mut Checked) -> Option<String> {
    let slot = plan
        .dataset_slot
        .clone()
        .or_else(|| plan.dataset_planned.then(|| PLANNED_SLOT.into()));
    if let Some(slot) = &slot {
        checked.data_resources.insert(format!("dataset:{slot}"));
    } else {
        checked.error("dataset", "Open a dataset before using its data.");
    }
    slot
}
fn bind_source(checked: &mut Checked, source: &DataSourceRef) {
    checked
        .data_resources
        .insert(format!("source:{}", source.reference));
    checked
        .data_resources
        .insert(format!("endpoint:{}", source.identity()));
    checked
        .facts
        .push(("Source".into(), super::redact::for_output_text(&source.uri)));
    checked
        .facts
        .push(("Source reference".into(), source.reference.clone()));
}
fn new_connection(ctx: &Context, checked: &mut Checked, source: &DataSourceRef) {
    bind_source(checked, source);
    if let Some(auth) = &source.auth {
        let label = ctx
            .data_profiles
            .iter()
            .find(|profile| profile.profile == auth.profile)
            .map_or("Saved local source", |profile| profile.name.as_str());
        checked.facts.push((
            "Local profile".into(),
            super::redact::for_output_text(label),
        ));
    }
    match crate::signals::parse(&source.uri) {
        Ok(parsed) => {
            if let Err(error) = crate::signals::require_compiled(parsed.protocol) {
                checked.issue(Issue::error(
                    Some("source"),
                    error.to_string(),
                    IssueCode::Unsupported,
                ));
            }
            if parsed.protocol != crate::signals::Protocol::SimSignal {
                checked.risk = Risk::Disruptive;
                checked.leaves_computer = true;
                checked
                    .reasons
                    .push("Contacts the approved sensor or PLC endpoint.".into());
            }
        }
        Err(error) => checked.error("source", error.to_string()),
    }
}

pub fn check(ctx: &Context, plan: &mut PlanState, checked: &mut Checked) {
    let result: Result<(), Vec<Issue>> = (|| {
        let spec = super::spec(&checked.call.tool).expect("registered data tool");
        let origin = plan.origin;
        if let Err(issue) = permit(ctx, origin) {
            return Err(vec![issue]);
        }
        let from_plan = plan.dataset_planned;
        let session = move |command: SessionCommand| {
            let dataset_bound = matches!(
                command,
                SessionCommand::DatasetCloseIf { .. }
                    | SessionCommand::DatasetNoteIf { .. }
                    | SessionCommand::DatasetQueryIf { .. }
                    | SessionCommand::DatasetModelQueryIf { .. }
                    | SessionCommand::DatasetExportIf { .. }
                    | SessionCommand::SignalCapture { .. }
            );
            Op::Session {
                command,
                dataset_from_plan: from_plan && dataset_bound,
            }
        };
        match spec.name {
            "browse_data_source" => {
                let args: BrowseArgs = parse_args(spec, &checked.call.args)?;
                if !(1..=1000).contains(&args.limit.unwrap_or(100)) {
                    checked.error("limit", "Choose a browse limit from 1 to 1,000.");
                }
                let endpoint = source(ctx, origin, &args.source).map_err(|issue| vec![issue])?;
                new_connection(ctx, checked, &endpoint);
                checked.ops.push(session(SessionCommand::SignalBrowse {
                    uri: endpoint.uri,
                    auth: endpoint.auth,
                    duration_ms: duration("timeout", &args.timeout, 30_000)
                        .map_err(|issue| vec![issue])?,
                    limit: args.limit.unwrap_or(100),
                }));
            }
            "read_data_source" => {
                let args: ReadArgs = parse_args(spec, &checked.call.args)?;
                let endpoint = source(ctx, origin, &args.source).map_err(|issue| vec![issue])?;
                new_connection(ctx, checked, &endpoint);
                checked.ops.push(session(SessionCommand::SignalRead {
                    uri: endpoint.uri,
                    auth: endpoint.auth,
                    timeout_ms: duration("timeout", &args.timeout, 30_000)
                        .map_err(|issue| vec![issue])?,
                }));
            }
            "open_dataset" => {
                let args: OpenArgs = parse_args(spec, &checked.call.args)?;
                if !args.file_name.ends_with(".duckdb") {
                    checked.error("file_name", "Use a .duckdb dataset file name.");
                }
                let path = super::values::capture_path(&ctx.capture_root, &args.file_name)
                    .map_err(|issue| vec![issue])?;
                super::values::contain_path(&ctx.capture_root, &path)
                    .map_err(|issue| vec![issue])?;
                if plan.dataset_slot.is_some()
                    && let Some(current) = &ctx.snapshot.dataset
                {
                    if Path::new(&current.path) != path {
                        checked.error("file_name","Close the current dataset before opening a different file. Closing the shared sample sink requires approval.");
                    } else {
                        if current.max_bytes != args.max_bytes {
                            checked.error("max_bytes", "This dataset is already open with another size policy. Use its current policy, or close it with approval before reopening.");
                        }
                        if args.mode == crate::dataset::OpenMode::Create {
                            checked.error("mode", "This dataset is already open. Choose open or auto to use it; create requires a new file.");
                        }
                    }
                }
                checked.output_root = Some(ctx.capture_root.clone());
                checked.ops.push(Op::Session {
                    command: SessionCommand::DatasetOpenIf {
                        expected_slot_id: plan.dataset_slot.clone(),
                        path: path.to_string_lossy().into_owned(),
                        mode: args.mode,
                        max_bytes: Some(args.max_bytes),
                    },
                    dataset_from_plan: false,
                });
                if let Some(slot) = &plan.dataset_slot {
                    checked.data_resources.insert(format!("dataset:{slot}"));
                } else {
                    checked.data_resources.insert("dataset:empty".into());
                }
                checked
                    .facts
                    .push(("Dataset".into(), super::redact::home_path(&path)));
                plan.dataset_slot = None;
                plan.dataset_planned = true;
            }
            "close_dataset" => {
                if let Some(slot_id) = dataset(plan, checked) {
                    let sink_generation = if from_plan {
                        None
                    } else if let Some(sink) = &ctx.snapshot.signal_sink {
                        checked
                            .data_resources
                            .insert(format!("sink:{}", sink.generation));
                        Some(sink.generation)
                    } else {
                        checked.error(
                            "dataset",
                            "Refresh the sample sink status before closing this dataset.",
                        );
                        None
                    };
                    checked.ops.push(session(SessionCommand::DatasetCloseIf {
                        slot_id,
                        sink_generation,
                    }));
                    plan.dataset_slot = None;
                    plan.dataset_planned = false;
                }
            }
            "start_signal_capture" => {
                let args: SignalArgs = parse_args(spec, &checked.call.args)?;
                let endpoint = source(ctx, origin, &args.source).map_err(|issue| vec![issue])?;
                new_connection(ctx, checked, &endpoint);
                if let Some(slot_id) = dataset(plan, checked) {
                    let sink_generation = if from_plan {
                        None
                    } else {
                        match ctx.snapshot.signal_sink.as_ref() {
                            Some(sink)
                                if sink.attached
                                    && sink.dataset_slot_id.as_ref() == Some(&slot_id) =>
                            {
                                checked
                                    .data_resources
                                    .insert(format!("sink:{}", sink.generation));
                                Some(sink.generation)
                            }
                            _ => {
                                checked.error("dataset", "The sample sink is not attached to this dataset. Open the dataset before starting capture.");
                                None
                            }
                        }
                    };
                    if ctx.snapshot.signals.iter().any(|source| {
                        source.id == endpoint.reference
                            && !matches!(
                                source.state,
                                crate::signals::SourceState::Stopped
                                    | crate::signals::SourceState::Failed
                            )
                    }) || plan.signal_captures.get(&endpoint.reference) == Some(&None)
                    {
                        checked.error("source","This source is already capturing. Review it before starting another capture.");
                    }
                    checked.ops.push(session(SessionCommand::SignalCapture {
                        slot_id,
                        sink_generation,
                        uri: endpoint.uri,
                        auth: endpoint.auth,
                        interval_ms: duration("every", &args.every, 3_600_000)
                            .map_err(|issue| vec![issue])?,
                        tags: args.tags.unwrap_or_default(),
                        duration_ms: duration("duration", &args.duration, 86_400_000)
                            .map_err(|issue| vec![issue])?,
                        max_samples: args.max_samples,
                        max_bytes: args.max_bytes,
                    }));
                    plan.signal_captures.insert(endpoint.reference, None);
                    plan.planned_bytes = plan.planned_bytes.saturating_add(args.max_bytes);
                    if ctx.task_data_reserved_bytes > 0 {
                        plan.prepaid_signal_payload_bytes = plan
                            .prepaid_signal_payload_bytes
                            .saturating_add(args.max_bytes);
                    }
                    checked
                        .facts
                        .push(("Payload limit".into(), super::format_bytes(args.max_bytes)));
                    checked
                        .facts
                        .push(("Sample limit".into(), args.max_samples.to_string()));
                }
            }
            "stop_signal_capture" | "wait_for_data" => {
                let (input, timeout_ms, samples) = if spec.name == "wait_for_data" {
                    let args: WaitArgs = parse_args(spec, &checked.call.args)?;
                    if args
                        .samples
                        .is_some_and(|samples| samples == 0 || samples > 100_000_000)
                    {
                        checked.error(
                            "samples",
                            "Choose a persisted sample count from 1 to 100,000,000.",
                        );
                    }
                    (
                        args.source,
                        Some(
                            duration("timeout", &args.timeout, 600_000)
                                .map_err(|issue| vec![issue])?,
                        ),
                        args.samples,
                    )
                } else {
                    let args: SourceArgs = parse_args(spec, &checked.call.args)?;
                    (args.source, None, None)
                };
                let reference = source_key(ctx, origin, &input).map_err(|issue| vec![issue])?;
                let Some(token) = plan.signal_captures.get(&reference).cloned() else {
                    checked.error(
                        "source",
                        "Start or inspect a source capture before stopping or waiting for it.",
                    );
                    return Ok(());
                };
                if origin == Origin::Assistant
                    && !ctx.data_sources.iter().any(|approved| {
                        approved.reference == reference
                            && (token.is_none()
                                || ctx
                                    .snapshot
                                    .signals
                                    .iter()
                                    .find(|status| status.id == reference)
                                    .is_some_and(|status| approved.permits_cached(status)))
                    })
                {
                    checked.error("source", "This capture belongs to a different or expired source binding. Select the source locally before using it.");
                    return Ok(());
                }
                checked.data_resources.insert(format!("source:{reference}"));
                if let Some(id) = &token {
                    checked.data_resources.insert(format!("signal:{id}"));
                }
                if let Some(timeout_ms) = timeout_ms {
                    checked.ops.push(Op::WaitData {
                        source: reference,
                        capture_id: token,
                        timeout_ms,
                        samples,
                    });
                } else {
                    if plan
                        .run
                        .as_ref()
                        .is_some_and(|run| run.signal_captures.get(&reference) == token.as_ref())
                    {
                        checked.risk = Risk::Bounded;
                    }
                    checked.ops.push(Op::StopSignal {
                        source: reference.clone(),
                        capture_id: token,
                    });
                    plan.signal_captures.remove(&reference);
                }
            }
            "annotate_dataset" => {
                let args: NoteArgs = parse_args(spec, &checked.call.args)?;
                if args.text.trim().is_empty() || args.text.len() > 4096 {
                    checked.error("text", "Use a note of 1 to 4,096 bytes.");
                }
                if let Some(slot_id) = dataset(plan, checked) {
                    checked.ops.push(session(SessionCommand::DatasetNoteIf {
                        slot_id,
                        text: args.text,
                        context: BTreeMap::new(),
                        camera: None,
                    }));
                }
            }
            "export_dataset" => {
                let args: ExportArgs = parse_args(spec, &checked.call.args)?;
                let Some(reserved_bytes) = args.max_bytes.checked_mul(2) else {
                    checked.error("max_bytes", "The bundle limit is too large to reserve both staged and published copies.");
                    return Ok(());
                };
                let Some(planned_bytes) = plan.planned_bytes.checked_add(reserved_bytes) else {
                    checked.error(
                        "max_bytes",
                        "The export reservation exceeds the available storage budget.",
                    );
                    return Ok(());
                };
                let output = super::values::capture_path(&ctx.capture_root, &args.folder)
                    .map_err(|mut issue| {
                        if issue.field.as_deref() == Some("file_name") {
                            issue.field = Some("folder".into());
                            issue.message = "Enter a new folder name inside the capture folder. Use a simple name without a path.".into();
                        }
                        vec![issue]
                    })?;
                super::values::contain_path(&ctx.capture_root, &output)
                    .map_err(|issue| vec![issue])?;
                if output.exists() {
                    checked.error(
                        "folder",
                        "Choose a new export folder; this folder already exists.",
                    );
                }
                if let Some(slot_id) = dataset(plan, checked) {
                    checked.output_root = Some(ctx.capture_root.clone());
                    checked.ops.push(session(SessionCommand::DatasetExportIf {
                        slot_id,
                        output: output.to_string_lossy().into_owned(),
                        approved_root: Some(
                            super::values::canonical_root(&ctx.capture_root)
                                .map_err(|issue| vec![issue])?
                                .to_string_lossy()
                                .into_owned(),
                        ),
                        formats: vec![args.format],
                        tables: args.tables.unwrap_or_default(),
                        since_ms: args.since_ms,
                        until_ms: args.until_ms,
                        storage: ctx.storage.request.clone(),
                        max_bytes: args.max_bytes,
                        timeout_ms: duration("timeout", &args.timeout, 600_000)
                            .map_err(|issue| vec![issue])?,
                    }));
                    plan.planned_bytes = planned_bytes;
                    checked
                        .facts
                        .push(("New folder".into(), super::redact::home_path(&output)));
                    checked
                        .facts
                        .push(("Bundle limit".into(), super::format_bytes(args.max_bytes)));
                    checked.facts.push((
                        "Data reservation (two copies)".into(),
                        super::format_bytes(reserved_bytes),
                    ));
                }
            }
            _ => checked.error("tool", "Choose a data action from tools list."),
        }
        if plan
            .planned_bytes
            .saturating_sub(plan.prepaid_signal_payload_bytes)
            > ctx.storage.free_budget()
        {
            checked.issue(Issue::error(
                Some("max_bytes"),
                "The data reservation exceeds the available storage budget.",
                IssueCode::StorageFull,
            ));
        }
        Ok(())
    })();
    if let Err(issues) = result {
        checked.issues.extend(issues);
    }
}

pub fn is_action(name: &str) -> bool {
    matches!(
        name,
        "browse_data_source"
            | "read_data_source"
            | "open_dataset"
            | "close_dataset"
            | "start_signal_capture"
            | "stop_signal_capture"
            | "wait_for_data"
            | "annotate_dataset"
            | "export_dataset"
    )
}
pub fn preflight(ctx: &Context, call: &ToolCall, origin: Origin) -> Result<(), Vec<Issue>> {
    if call.tool == "query_dataset" {
        permit(ctx, origin).map_err(|issue| vec![issue])?;
        let args: QueryArgs = parse_args(&QUERY_DATASET, &call.args)?;
        duration("timeout", &args.timeout, 30_000).map_err(|issue| vec![issue])?;
        if !(1..=1000).contains(&args.limit.unwrap_or(100)) {
            return Err(vec![Issue::usage(
                Some("limit"),
                "Use a row limit from 1 to 1,000.",
            )]);
        }
        if ctx
            .snapshot
            .dataset
            .as_ref()
            .and_then(|dataset| dataset.slot_id.as_ref())
            .is_none()
        {
            return Err(vec![Issue::usage(
                None,
                "Open a dataset before querying it.",
            )]);
        }
        if origin == Origin::Assistant
            && ctx
                .snapshot
                .dataset
                .as_ref()
                .and_then(|dataset| dataset.slot_id.as_ref())
                != ctx.data_dataset.as_ref()
        {
            return Err(vec![Issue::usage(
                None,
                "This dataset is outside the task's approved scope. Review the current dataset first.",
            )]);
        }
    }
    Ok(())
}
pub fn metadata(ctx: &Context, origin: Origin) -> Value {
    let allowed = origin == Origin::Cli || ctx.allow_data;
    let dataset = ctx.snapshot.dataset.as_ref().filter(|dataset| {
        origin == Origin::Cli
            || ctx.allow_data
                && ctx
                    .data_dataset
                    .as_ref()
                    .is_some_and(|slot| dataset.slot_id.as_ref() == Some(slot))
    });
    let sources = ctx.snapshot.signals.iter()
        .filter(|source| origin == Origin::Cli || ctx.data_sources.iter().any(|approved| approved.permits_cached(source)))
        .map(|source| json!({"reference":source.id,"capture_id":source.capture_id,"state":source.state,"samples":source.samples,"written":source.written,"dropped":source.dropped,"error":allowed.then_some(&source.error),"persistence_complete":source.persistence_complete,"persistence_error":allowed.then_some(&source.persistence_error),"payload_bytes":source.payload_bytes,"written_bytes":source.written_bytes}))
        .collect::<Vec<_>>();
    let sink = ctx.snapshot.signal_sink.as_ref().filter(|sink| {
        origin == Origin::Cli || ctx.allow_data && ctx.data_dataset.as_ref().is_some_and(|slot|sink.dataset_slot_id.as_ref()==Some(slot))
    }).map(|sink| json!({"attached":sink.attached,"dataset_slot_id":sink.dataset_slot_id,"generation":sink.generation,"dropped_total":sink.dropped_total,"error":allowed.then_some(&sink.error)}));
    let mut metadata = json!({"dataset":dataset,"source_count":sources.len(),"sources":sources,"signal_sink":sink,"compiled_protocols":crate::signals::compiled_protocols(),"data_permission":ctx.allow_data});
    if ctx.task_data_reserved_bytes > 0 && dataset.is_some() {
        metadata["task_dataset_storage"] = json!({"reserved_logical_bytes":ctx.task_data_reserved_bytes,"signal_workspace_prepaid":true,"exports_need_shared_storage":true});
    }
    metadata
}
pub fn read(
    host: &dyn ToolHost,
    ctx: &Context,
    call: &ToolCall,
    origin: Origin,
    max_wait: Duration,
) -> Result<Value, Vec<Issue>> {
    preflight(ctx, call, origin)?;
    match call.tool.as_str() {
        "get_dataset" => Ok(
            json!({"dataset":metadata(ctx,origin)["dataset"],"signal_sink":metadata(ctx,origin)["signal_sink"],"tables":["dataset_info","captures","recordings","signals","samples","events","captures_with_signals"],"data_permission":ctx.allow_data}),
        ),
        "list_data_sources" => {
            let mut sources = serde_json::to_value(
                ctx.snapshot
                    .signals
                    .iter()
                    .filter(|source| {
                        origin == Origin::Cli
                            || ctx
                                .data_sources
                                .iter()
                                .any(|approved| approved.permits_cached(source))
                    })
                    .collect::<Vec<_>>(),
            )
            .unwrap_or_else(|_| json!([]));
            for source in sources.as_array_mut().expect("source array") {
                source
                    .as_object_mut()
                    .expect("source object")
                    .remove("auth");
            }
            if origin == Origin::Assistant && !ctx.allow_data {
                for source in sources.as_array_mut().expect("source array") {
                    source
                        .as_object_mut()
                        .expect("source object")
                        .remove("tags");
                    source["error"] = Value::Null;
                    source["persistence_error"] = Value::Null;
                }
            }
            Ok(
                json!({"sources":sources,"signal_sink":metadata(ctx,origin)["signal_sink"],"compiled_protocols":crate::signals::compiled_protocols(),"approved":ctx.data_sources.iter().map(|source|json!({"reference":source.reference,"location":super::redact::for_model_text(&source.uri),"saved_profile":source.auth.is_some()})).collect::<Vec<_>>(),"bound":(origin==Origin::Cli).then(|| ctx.snapshot.signal_bindings.iter().map(|receipt|json!({"reference":receipt.reference,"location":super::redact::for_model_text(&receipt.endpoint),"saved_profile":true})).collect::<Vec<_>>()),"values_allowed":origin==Origin::Cli||ctx.allow_data}),
            )
        }
        "query_dataset" => {
            let args: QueryArgs = parse_args(&QUERY_DATASET, &call.args)?;
            let slot_id = ctx
                .snapshot
                .dataset
                .as_ref()
                .and_then(|dataset| dataset.slot_id.clone())
                .expect("preflight dataset");
            let timeout_ms = duration("timeout", &args.timeout, 30_000)
                .map_err(|issue| vec![issue])?
                .min(max_wait.as_millis().try_into().unwrap_or(u64::MAX));
            if timeout_ms == 0 {
                return Err(vec![Issue::error(
                    None,
                    "The task deadline was reached before querying data.",
                    IssueCode::Timeout,
                )]);
            }
            let command = if origin == Origin::Assistant {
                SessionCommand::DatasetModelQueryIf {
                    slot_id,
                    sql: args.sql,
                    limit: args.limit.unwrap_or(100),
                    timeout_ms,
                    max_result_bytes: QUERY_BYTES,
                }
            } else {
                SessionCommand::DatasetQueryIf {
                    slot_id,
                    sql: args.sql,
                    limit: args.limit.unwrap_or(100),
                    timeout_ms,
                    max_result_bytes: QUERY_BYTES,
                }
            };
            host.execute(None, command)
                .map_err(|error| vec![Issue::failed(&error, &call.args, origin)])
        }
        _ => Err(vec![Issue::usage(None, "Choose a data observation tool.")]),
    }
}
pub fn bind_dataset(command: &mut SessionCommand, slot: &str) -> Result<()> {
    match command {
        SessionCommand::DatasetCloseIf { slot_id, .. }
        | SessionCommand::DatasetNoteIf { slot_id, .. }
        | SessionCommand::DatasetQueryIf { slot_id, .. }
        | SessionCommand::DatasetModelQueryIf { slot_id, .. }
        | SessionCommand::DatasetExportIf { slot_id, .. }
        | SessionCommand::SignalCapture { slot_id, .. } => *slot_id = slot.into(),
        _ => bail!("This operation cannot use a dataset from an earlier plan step."),
    }
    Ok(())
}
pub fn contain_output(root: Option<&Path>, command: &SessionCommand) -> Result<()> {
    if let Some(root) = root {
        let output = match command {
            SessionCommand::DatasetOpenIf { path, .. } => Some(path),
            SessionCommand::DatasetExportIf { output, .. } => Some(output),
            _ => None,
        };
        if let Some(output) = output {
            super::values::contain_path(root, Path::new(output))
                .map_err(|issue| anyhow::anyhow!(issue.message))?;
        }
    }
    Ok(())
}
pub fn acknowledge(
    reply: &Value,
    dataset: &mut Option<String>,
    signals: &mut BTreeMap<String, String>,
) {
    if let Some(slot) = reply["slot_id"]
        .as_str()
        .or_else(|| reply["dataset"]["slot_id"].as_str())
    {
        *dataset = Some(slot.into());
    }
    for source in [reply.get("source"), Some(reply)].into_iter().flatten() {
        if let (Some(id), Some(token)) = (source["id"].as_str(), source["capture_id"].as_str()) {
            signals.insert(id.into(), token.into());
        }
    }
}
pub fn wait(
    host: &dyn ToolHost,
    source: &str,
    capture: &str,
    timeout_ms: u64,
    samples: Option<u64>,
    cancel: &AtomicBool,
) -> Result<Value> {
    let started = Instant::now();
    loop {
        ensure!(
            !cancel.load(Ordering::Relaxed),
            "Waiting for data was stopped."
        );
        let snapshot = host.snapshot()?;
        let status = snapshot
            .signals
            .iter()
            .find(|state| state.id == source)
            .context(
                "The signal capture is no longer present. Inspect sources before continuing.",
            )?;
        ensure!(
            status.capture_id == capture,
            "The signal capture was replaced. Inspect it before continuing."
        );
        ensure!(
            status.persistence_error.is_none(),
            "Signal persistence failed: {}",
            status
                .persistence_error
                .as_deref()
                .unwrap_or("unknown error")
        );
        ensure!(
            status.state != crate::signals::SourceState::Failed,
            "Signal capture failed: {}",
            status.error.as_deref().unwrap_or("unknown error")
        );
        let complete = status.persistence_complete;
        let satisfied = samples.map_or(complete, |count| status.written >= count);
        if satisfied {
            ensure!(
                status.dropped == 0
                    && status.helper_dropped == 0
                    && (!complete || status.samples == status.written),
                "The capture has missing or unpersisted samples. Inspect its counts before using the dataset."
            );
            return Ok(
                json!({"satisfied":true,"source":status,"waited_ms":started.elapsed().as_millis()}),
            );
        }
        ensure!(
            !complete,
            "Capture completed before the requested persisted sample count."
        );
        ensure!(
            started.elapsed() < Duration::from_millis(timeout_ms),
            "Timed out waiting for persisted sensor data."
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{
        StepId,
        tests::{FakeHost, fixture_context, fixture_snapshot},
    };
    fn call(tool: &str, args: Value) -> ToolCall {
        ToolCall {
            tool: tool.into(),
            args,
        }
    }
    fn context() -> Context {
        let mut ctx = fixture_context(fixture_snapshot());
        ctx.capture_root = std::env::temp_dir().join("capturefab-data-tool-checks");
        ctx.allow_data = true;
        ctx.data_sources = vec![DataSourceRef::from_uri("sim-signal://line1").unwrap()];
        ctx
    }
    fn sample() -> Value {
        json!({"source":"sim-signal://line1","every":"100ms","duration":"2s","max_samples":20,"max_bytes":65536,"tags":null})
    }
    #[test]
    fn data_plan_pins_prior_open_capture_and_wait_without_executing() {
        let ctx = context();
        let mut plan = PlanState::new(&ctx);
        let steps = [
            call(
                "open_dataset",
                json!({"file_name":"plant.duckdb","mode":"create","max_bytes":134217728}),
            ),
            call(
                "read_data_source",
                json!({"source":"sim-signal://line1","timeout":"2s"}),
            ),
            call("start_signal_capture", sample()),
            call(
                "wait_for_data",
                json!({"source":"sim-signal://line1","timeout":"5s","samples":null}),
            ),
            call(
                "export_dataset",
                json!({"folder":"export","format":"parquet","tables":null,"since_ms":null,"until_ms":null,"max_bytes":1048576,"timeout":"10s"}),
            ),
        ];
        let checked: Vec<_> = steps
            .iter()
            .enumerate()
            .map(|(index, call)| {
                crate::tools::check(&ctx, &mut plan, StepId(index as u64 + 1), call, &|_, _| {
                    false
                })
            })
            .collect();
        assert!(checked.iter().all(Checked::ready), "{checked:?}");
        assert!(matches!(
            &checked[1].ops[0],
            Op::Session {
                dataset_from_plan: false,
                ..
            }
        ));
        assert!(
            matches!(&checked[2].ops[0],Op::Session{dataset_from_plan:true,command:SessionCommand::SignalCapture{slot_id,..}} if slot_id==PLANNED_SLOT)
        );
        assert!(matches!(
            &checked[3].ops[0],
            Op::WaitData {
                capture_id: None,
                ..
            }
        ));
        assert_eq!(plan.planned_bytes, 65536 + 2 * 1048576);
        assert_eq!(
            crate::tools::spec("read_data_source").unwrap().kind,
            ToolKind::Control
        );
        assert_eq!(
            crate::tools::spec("browse_data_source").unwrap().kind,
            ToolKind::Control
        );
    }

    #[test]
    fn export_review_reserves_two_copies_without_changing_the_bundle_limit() {
        let mut ctx = context();
        ctx.storage.effective.max_bytes = 199;
        ctx.storage.allocated_bytes = 0;
        ctx.storage.reserved_bytes = 0;
        for (bundle_bytes, ready) in [(99, true), (100, false), (u64::MAX, false)] {
            let mut plan = PlanState::new(&ctx);
            plan.dataset_slot = Some(PLANNED_SLOT.into());
            let checked = crate::tools::check(
                &ctx,
                &mut plan,
                StepId(1),
                &call(
                    "export_dataset",
                    json!({"folder":"peak-reserved-export","format":"csv","tables":null,"since_ms":null,"until_ms":null,"max_bytes":bundle_bytes,"timeout":"1s"}),
                ),
                &|_, _| false,
            );
            assert_eq!(checked.ready(), ready, "{checked:?}");
            if ready {
                assert_eq!(plan.planned_bytes, 2 * bundle_bytes);
                assert!(
                    matches!(&checked.ops[0], Op::Session { command: SessionCommand::DatasetExportIf { max_bytes, .. }, .. } if *max_bytes == bundle_bytes)
                );
                assert!(
                    checked
                        .facts
                        .iter()
                        .any(|(name, _)| name == "Data reservation (two copies)")
                );
            } else if bundle_bytes == u64::MAX {
                assert!(checked.ops.is_empty());
                assert_eq!(plan.planned_bytes, 0);
            }
        }
    }

    #[test]
    fn private_signal_payload_uses_its_prepaid_workspace_while_exports_still_need_global_room() {
        let mut ctx = context();
        let slot = "a".repeat(32);
        ctx.snapshot.dataset = Some(crate::dataset::DatasetStatus {
            path: "task.duckdb".into(),
            slot_id: Some(slot.clone()),
            max_bytes: 16 << 20,
            ..Default::default()
        });
        ctx.snapshot.signal_sink = Some(crate::signals::SignalSinkStatus {
            attached: true,
            generation: 7,
            dataset_slot_id: Some(slot.clone()),
            ..Default::default()
        });
        ctx.data_dataset = Some(slot);
        ctx.task_data_reserved_bytes = 16 << 20;
        ctx.storage.effective.max_bytes = 1;
        ctx.storage.allocated_bytes = 1;
        ctx.storage.reserved_bytes = 0;
        let mut plan = PlanState::new(&ctx);
        let capture = crate::tools::check(
            &ctx,
            &mut plan,
            StepId(1),
            &call("start_signal_capture", sample()),
            &|_, _| false,
        );
        assert!(capture.ready(), "{capture:?}");
        let export = crate::tools::check(
            &ctx,
            &mut plan,
            StepId(2),
            &call(
                "export_dataset",
                json!({"folder":"still-needs-space","format":"csv","tables":null,"since_ms":null,"until_ms":null,"max_bytes":100,"timeout":"1s"}),
            ),
            &|_, _| false,
        );
        assert!(!export.ready());
        assert!(
            export
                .issues
                .iter()
                .any(|issue| issue.message.contains("available storage budget"))
        );
        ctx.task_data_reserved_bytes = 0;
        let mut shared = PlanState::new(&ctx);
        let capture = crate::tools::check(
            &ctx,
            &mut shared,
            StepId(3),
            &call("start_signal_capture", sample()),
            &|_, _| false,
        );
        assert!(
            !capture.ready(),
            "a shared sink has no prepaid task workspace"
        );
    }
    #[test]
    fn planned_capture_uses_the_open_receipt_and_refuses_a_missing_sink_acknowledgement() {
        let ctx = context();
        let mut plan = PlanState::new(&ctx);
        let open = crate::tools::check(
            &ctx,
            &mut plan,
            StepId(1),
            &call(
                "open_dataset",
                json!({"file_name":"receipt.duckdb","mode":"create","max_bytes":134217728}),
            ),
            &|_, _| false,
        );
        let capture = crate::tools::check(
            &ctx,
            &mut plan,
            StepId(2),
            &call("start_signal_capture", sample()),
            &|_, _| false,
        );
        assert!(open.ready() && capture.ready());
        for generation in [None, Some(17u64)] {
            let host = FakeHost::new(ctx.snapshot.clone()).reply(
                "dataset_open_if",
                Ok(json!({"slot_id":"a".repeat(32),"sink_generation":generation})),
            );
            let mut executor =
                crate::tools::exec::Executor::new(&host, &crate::tools::exec::EmptySecrets);
            let cancel = AtomicBool::new(false);
            executor.run_step(&open, &cancel).unwrap();
            let result = executor.run_step(&capture, &cancel);
            if generation.is_some() {
                result.unwrap();
                assert!(
                    matches!(&host.calls()[1].1, SessionCommand::SignalCapture { slot_id, sink_generation:Some(17), .. } if slot_id == &"a".repeat(32))
                );
            } else {
                assert!(result.unwrap_err().to_string().contains("sample sink"));
                assert_eq!(host.calls().len(), 1);
            }
        }
    }
    #[test]
    fn opening_an_existing_slot_cannot_claim_a_new_size_policy_or_new_file() {
        let mut ctx = context();
        ctx.snapshot.dataset = Some(crate::dataset::DatasetStatus {
            slot_id: Some("a".repeat(32)),
            path: ctx
                .capture_root
                .join("existing.duckdb")
                .to_string_lossy()
                .into_owned(),
            max_bytes: 134217728,
            ..Default::default()
        });
        for (mode, max_bytes, ready) in [
            ("auto", 16777216, false),
            ("create", 134217728, false),
            ("open", 134217728, true),
        ] {
            let mut plan = PlanState::new(&ctx);
            let checked = crate::tools::check(
                &ctx,
                &mut plan,
                StepId(1),
                &call(
                    "open_dataset",
                    json!({"file_name":"existing.duckdb","mode":mode,"max_bytes":max_bytes}),
                ),
                &|_, _| false,
            );
            assert_eq!(checked.ready(), ready, "{checked:?}");
        }
    }
    #[test]
    fn assistant_cannot_invent_endpoints_or_query_without_exact_data_consent() {
        let mut ctx = context();
        let source = ctx.data_sources[0].reference.clone();
        let mut plan = PlanState::assistant(&ctx);
        let attempted = crate::tools::check(
            &ctx,
            &mut plan,
            StepId(1),
            &call(
                "read_data_source",
                json!({"source":"sim-signal://invented","timeout":"2s"}),
            ),
            &|_, _| false,
        );
        assert!(!attempted.ready());
        assert!(attempted.ops.is_empty());
        let accepted = crate::tools::check(
            &ctx,
            &mut plan,
            StepId(2),
            &call("read_data_source", json!({"source":source,"timeout":"2s"})),
            &|_, _| false,
        );
        assert!(accepted.ready(), "{accepted:?}");
        ctx.snapshot.dataset = Some(crate::dataset::DatasetStatus {
            slot_id: Some("a".repeat(32)),
            ..Default::default()
        });
        let query = call(
            "query_dataset",
            json!({"sql":"SELECT 1","limit":10,"timeout":"2s"}),
        );
        assert!(preflight(&ctx, &query, Origin::Assistant).is_err());
        ctx.data_dataset = Some("a".repeat(32));
        assert!(preflight(&ctx, &query, Origin::Assistant).is_ok());
        ctx.allow_data = false;
        let host = FakeHost::new(ctx.snapshot.clone());
        assert!(
            read(
                &host,
                &ctx,
                &query,
                Origin::Assistant,
                Duration::from_secs(2)
            )
            .is_err()
        );
        assert!(host.calls().is_empty());
    }
    #[test]
    fn model_queries_use_the_private_projection_and_cli_queries_keep_local_data() {
        let mut ctx = context();
        let slot = "a".repeat(32);
        ctx.snapshot.dataset = Some(crate::dataset::DatasetStatus {
            slot_id: Some(slot.clone()),
            ..Default::default()
        });
        ctx.data_dataset = Some(slot.clone());
        let query = call(
            "query_dataset",
            json!({"sql":"SELECT serial AS value FROM captures","limit":3,"timeout":"5s"}),
        );
        for origin in [Origin::Assistant, Origin::Cli] {
            let host = FakeHost::new(ctx.snapshot.clone());
            read(&host, &ctx, &query, origin, Duration::from_secs(2)).unwrap();
            let calls = host.calls();
            assert_eq!(calls.len(), 1);
            let command = &calls[0].1;
            if origin == Origin::Assistant {
                assert!(matches!(
                    command,
                    SessionCommand::DatasetModelQueryIf { .. }
                ));
            } else {
                assert!(matches!(command, SessionCommand::DatasetQueryIf { .. }));
            }
            let wire = serde_json::to_value(command).unwrap();
            assert_eq!(wire["slot_id"], slot);
            assert_eq!(wire["sql"], "SELECT serial AS value FROM captures");
            assert_eq!(wire["limit"], 3);
            assert_eq!(wire["timeout_ms"], 2000);
            assert_eq!(wire["max_result_bytes"], QUERY_BYTES);
        }
    }
    #[test]
    fn human_approval_distinguishes_register_paths_while_model_facts_redact_them() {
        let mut facts = Vec::new();
        for address in [0, 1] {
            let endpoint = DataSourceRef::from_uri(&format!(
                "modbus-tcp://10.0.0.20/1/holding/{address}?type=f32&name=temp_c"
            ))
            .unwrap();
            let mut checked = Checked::new(StepId(1), &call("read_data_source", json!({})));
            bind_source(&mut checked, &endpoint);
            let human = serde_json::to_value(&checked.facts).unwrap();
            assert!(human.to_string().contains(&format!("/holding/{address}")));
            let model = super::super::redact::for_model(human.clone());
            assert!(!model.to_string().contains("/holding/"));
            facts.push(human);
        }
        assert_ne!(facts[0], facts[1]);
    }
    #[test]
    fn model_data_views_omit_unapproved_sources_and_dataset() {
        let mut ctx = context();
        let id = ctx.data_sources[0].reference.clone();
        let source = |id: &str, value: &str| {
            serde_json::from_value(json!({"id":id,"capture_id":"b".repeat(32),"protocol":"sim-signal","uri":"sim-signal://redacted","interval_ms":100,"state":"running","started_ms":0,"tags":[{"tag":"temperature","unit":"C","value":value,"quality":"good","ts_ms":1}]})).unwrap()
        };
        ctx.snapshot.signals = vec![
            source(&id, "allowed-reading"),
            source("signal:0000000000000000", "private-reading"),
        ];
        ctx.snapshot.dataset = Some(crate::dataset::DatasetStatus {
            slot_id: Some("c".repeat(32)),
            name: "private-dataset".into(),
            ..Default::default()
        });
        let host = FakeHost::new(ctx.snapshot.clone());
        let result = read(
            &host,
            &ctx,
            &call("list_data_sources", json!({})),
            Origin::Assistant,
            Duration::from_secs(2),
        )
        .unwrap()
        .to_string();
        assert!(result.contains("allowed-reading"));
        assert!(!result.contains("private-reading"));
        assert!(
            !metadata(&ctx, Origin::Assistant)
                .to_string()
                .contains("private-dataset")
        );
        ctx.allow_data = false;
        let result = read(
            &host,
            &ctx,
            &call("list_data_sources", json!({})),
            Origin::Assistant,
            Duration::from_secs(2),
        )
        .unwrap()
        .to_string();
        assert!(!result.contains("allowed-reading"));
    }

    #[test]
    fn cached_values_and_waits_do_not_cross_profile_accounts_at_one_endpoint() {
        let mut ctx = context();
        let approved = crate::tools::tests::fixture_bound_source(&mut ctx, "mqtts://host/topic");
        ctx.data_sources = vec![approved.clone()];
        let mut status: crate::signals::SourceStatus = serde_json::from_value(json!({"id":approved.reference,"auth":approved.auth,"capture_id":"d".repeat(32),"protocol":"mqtt","uri":"mqtts://host/topic","interval_ms":100,"state":"running","started_ms":0,"tags":[{"tag":"temperature","value":"other-account-value","quality":"good","ts_ms":1}]})).unwrap();
        status.auth.as_mut().unwrap().epoch = "e".repeat(32);
        ctx.snapshot.signals = vec![status];
        let host = FakeHost::new(ctx.snapshot.clone());
        let result = read(
            &host,
            &ctx,
            &call("list_data_sources", json!({})),
            Origin::Assistant,
            Duration::from_secs(2),
        )
        .unwrap();
        assert!(result["sources"].as_array().unwrap().is_empty());
        assert_eq!(metadata(&ctx, Origin::Assistant)["source_count"], 0);
        let wait = call(
            "wait_for_data",
            json!({"source":approved.reference,"timeout":"1s","samples":null}),
        );
        let checked = crate::tools::check(
            &ctx,
            &mut PlanState::assistant(&ctx),
            StepId(1),
            &wait,
            &|_, _| false,
        );
        assert!(!checked.ready());
        assert!(checked.ops.is_empty());
        ctx.snapshot.signals[0].auth = approved.auth.clone();
        let result = read(
            &host,
            &ctx,
            &call("list_data_sources", json!({})),
            Origin::Assistant,
            Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(result["sources"].as_array().unwrap().len(), 1);
        assert!(result["sources"][0].get("auth").is_none());
        assert!(!result.to_string().contains(&"c".repeat(32)));
        let checked = crate::tools::check(
            &ctx,
            &mut PlanState::assistant(&ctx),
            StepId(1),
            &wait,
            &|_, _| false,
        );
        assert!(checked.ready(), "{checked:?}");
    }
    #[test]
    fn replacement_or_missing_persistence_cannot_satisfy_a_data_wait() {
        let mut ctx = context();
        let id = ctx.data_sources[0].reference.clone();
        let mut status:crate::signals::SourceStatus=serde_json::from_value(json!({"id":id,"capture_id":"b".repeat(32),"protocol":"sim-signal","uri":"sim-signal://line1","interval_ms":100,"state":"stopped","started_ms":0,"samples":4,"written":3,"persistence_complete":true})).unwrap();
        ctx.snapshot.signals = vec![status.clone()];
        let cancel = AtomicBool::new(false);
        assert!(
            wait(
                &FakeHost::new(ctx.snapshot.clone()),
                &id,
                &"a".repeat(32),
                1000,
                None,
                &cancel
            )
            .unwrap_err()
            .to_string()
            .contains("replaced")
        );
        assert!(
            wait(
                &FakeHost::new(ctx.snapshot.clone()),
                &id,
                &status.capture_id,
                1000,
                None,
                &cancel
            )
            .unwrap_err()
            .to_string()
            .contains("unpersisted")
        );
        status.written = 4;
        ctx.snapshot.signals = vec![status.clone()];
        assert!(
            wait(
                &FakeHost::new(ctx.snapshot),
                &id,
                &status.capture_id,
                1000,
                None,
                &cancel
            )
            .is_ok()
        );
    }
}
