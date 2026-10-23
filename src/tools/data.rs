//! Checked camera dataset operations shared by every agent.
use super::{
    Checked, FieldKind, FieldSpec, Issue, IssueCode, Op, Origin, PlanState, Risk, ToolCall,
    ToolKind, ToolSpec,
    context::{Context, ToolHost},
    parse_args, schema,
};
use crate::{
    dataset::{ExportFormat, OpenMode},
    session::SessionCommand,
};
use anyhow::{Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path, time::Duration};

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
pub const GET_DATASET: ToolSpec = spec(
    "get_dataset",
    "Inspect dataset",
    ToolKind::Read,
    Risk::Observe,
    "Inspect the current dataset identity, row counts, size, limits and errors, plus the standard table names. Does not query rows or open a file. Use query_dataset for read-only SQL and export_dataset for an analysis bundle.",
    schema::no_arguments,
    &[],
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
    "Close only the exact reviewed dataset instance. This may affect other work, so approval is required. The database and saved captures remain on disk.",
    schema::no_arguments,
    &[],
);
pub const QUERY_DATASET: ToolSpec = spec(
    "query_dataset",
    "Query dataset",
    ToolKind::Read,
    Risk::Observe,
    "Run one read-only SELECT against the exact current dataset, timeout 1s..30s, limit 1..1000 (100 by default). Built-in model queries first copy the standard tables into a bounded private snapshot, removing camera serials and private locator details before SQL runs. A snapshot beyond 100,000 rows or 16 MiB of transferred data fails explicitly. Results have a 16 KiB cap with truncation markers. Requires permission to send data to the model. Use get_dataset for table names;",
    query_schema,
    QUERY_FIELDS,
);
pub const ANNOTATE_DATASET: ToolSpec = spec(
    "annotate_dataset",
    "Add dataset note",
    ToolKind::Control,
    Risk::Bounded,
    "Save a timestamped note of at most 4,096 bytes in the exact reviewed dataset. Does not change captured files or camera settings.",
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
fn permit(ctx: &Context, origin: Origin) -> Result<(), Issue> {
    if origin == Origin::Assistant && !ctx.allow_data {
        return Err(Issue::usage(
            None,
            "Allow data access before sending camera dataset rows or notes to the assistant.",
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
            );
            Op::Session {
                command,
                dataset_from_plan: from_plan && dataset_bound,
            }
        };
        match spec.name {
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
                        checked.error("file_name","Close the current dataset before opening a different file. Closing the shared dataset requires approval.");
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
                    checked
                        .ops
                        .push(session(SessionCommand::DatasetCloseIf { slot_id }));
                    plan.dataset_slot = None;
                    plan.dataset_planned = false;
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
        if plan.planned_bytes > ctx.storage.free_budget() {
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
        "open_dataset" | "close_dataset" | "annotate_dataset" | "export_dataset"
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
    let dataset = ctx.snapshot.dataset.as_ref().filter(|dataset| {
        origin == Origin::Cli
            || ctx.allow_data
                && ctx
                    .data_dataset
                    .as_ref()
                    .is_some_and(|slot| dataset.slot_id.as_ref() == Some(slot))
    });
    json!({"dataset":dataset,"data_permission":ctx.allow_data})
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
            json!({"dataset":metadata(ctx,origin)["dataset"],"tables":["dataset_info","captures","recordings","events"],"data_permission":ctx.allow_data}),
        ),
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
        | SessionCommand::DatasetExportIf { slot_id, .. } => *slot_id = slot.into(),
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
pub fn acknowledge(reply: &Value, dataset: &mut Option<String>) {
    if let Some(slot) = reply["slot_id"]
        .as_str()
        .or_else(|| reply["dataset"]["slot_id"].as_str())
    {
        *dataset = Some(slot.into());
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
        ctx
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
}
