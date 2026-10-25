//! One registry of camera tools for the in-app assistant and for external
//! agents (`capturefab tools`): names, portable JSON Schemas, strict argument
//! parsing, risk classes and executors. Always compiled, so headless builds
//! serve exactly the same tools.
//!
//! Observation tools (`read`) only read. They never connect, select, set,
//! start or stop anything, and their results are bounded and redacted for
//! whoever reads them: `redact::for_output` for people and command-line
//! callers, `redact::for_model` for AI providers.
mod arguments;
pub mod capture;
pub mod check;
pub mod context;
pub mod control;
pub mod data;
pub mod destinations;
pub mod exec;
pub mod fields;
pub mod plan;
pub mod read;
pub mod redact;
pub mod risk;
pub mod schema;
pub mod setups;
pub mod values;
pub use check::{check, check_plan, check_plan_with};
pub use exec::{
    ApplyError, CheckFailure, Checked, EmptySecrets, Executor, Op, PlanFailure, PlanState,
    Progress, RunFacts, SecretSource, StepOutcome, apply,
};
#[cfg(test)]
pub(crate) mod tests;

use crate::types::{CameraInfo, Transport};
use context::Context;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

/// The plan document version (`{"version":1,"calls":[…]}`).
pub const PLAN_VERSION: u32 = 1;
/// Most calls one plan may hold.
pub const MAX_PLAN_CALLS: usize = 64;
/// Largest tool result, as compact JSON. Longer lists are cut with a marker.
pub const MAX_OUTPUT_BYTES: usize = 24 * 1024;

/// How catalogs group tools.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    Read,
    Control,
    Capture,
    Configure,
}

/// What running a tool can do, in increasing order. A tool's spec holds its
/// minimum; checking a call may raise it. Operations no tool may perform
/// (deleting data, handling secrets, changing storage policy) are not tools.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    /// Reads only.
    Observe,
    /// Changes that undo restores.
    Reversible,
    /// Writes files or starts work within the storage budget.
    Bounded,
    /// Ends or redirects someone's work, or sends data off this computer.
    Disruptive,
    /// Destination changes: a person always approves these.
    HumanOnly,
}

/// One tool: what catalogs, forms and providers are told about it.
#[derive(Clone, Copy, Debug)]
pub struct ToolSpec {
    pub name: &'static str,
    pub title: &'static str,
    pub kind: ToolKind,
    /// The minimum class; checking a call may raise it.
    pub risk: Risk,
    /// The tool cannot run without a running session.
    pub needs_session: bool,
    /// The tool is useful only to a model that can see images.
    pub needs_vision: bool,
    /// For models and people: ASCII, at most 1024 bytes, stating ranges,
    /// defaults and caveats.
    pub description: &'static str,
    /// The arguments' JSON Schema, in the portable subset `schema` builds.
    pub schema: fn() -> Value,
    /// Form metadata for each argument, plus form-only fields.
    pub fields: &'static [FieldSpec],
}

/// How a form shows one argument.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FieldSpec {
    pub key: &'static str,
    pub label: &'static str,
    pub kind: FieldKind,
    /// Null (the default) is allowed.
    pub optional: bool,
    pub help: &'static str,
    /// Typed into a form or the CLI only; never in schemas or sent to a model.
    pub ui_only: bool,
}

/// The input a form uses for an argument.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FieldKind {
    Camera,
    Destination,
    /// A saved camera setup's name.
    Setup,
    Duration,
    Time,
    Integer {
        min: i64,
        max: i64,
    },
    Number {
        min: f64,
        max: f64,
    },
    Text,
    FileName,
    /// Allowed values with their labels.
    Choice(&'static [(&'static str, &'static str)]),
    Bool,
    Url,
    Path,
    Tags,
    /// A JSON array of strings, edited as one name per row.
    StringList,
    Secret,
    Features,
    Region,
}

/// Every tool, in catalog order. Each tool module defines its own specs;
/// add new tools here.
static CATALOG: &[ToolSpec] = &[
    read::GET_OVERVIEW,
    read::LIST_CAMERAS,
    read::LIST_DESTINATIONS,
    read::GET_STORAGE,
    read::LIST_JOBS,
    read::GET_CAMERA_FEATURES,
    setups::LIST_CAMERA_SETUPS,
    setups::PREVIEW_CAMERA_SETUP,
    read::INSPECT_FRAME,
    read::WAIT_FOR,
    data::GET_DATASET,
    data::QUERY_DATASET,
    data::OPEN_DATASET,
    data::CLOSE_DATASET,
    data::ANNOTATE_DATASET,
    data::EXPORT_DATASET,
    control::CONNECT_CAMERA,
    control::START_LIVE_VIEW,
    control::STOP_LIVE_VIEW,
    control::SET_CAMERA_FEATURES,
    control::SET_CAMERA_ROI,
    control::SET_AUTO_EXPOSURE,
    control::RUN_CAMERA_COMMAND,
    control::UNDO_CAMERA_CHANGE,
    setups::APPLY_CAMERA_SETUP,
    capture::CAPTURE_PHOTOS,
    capture::START_RECORDING,
    capture::STOP_RECORDING,
    capture::SCHEDULE_CAPTURES,
    capture::CANCEL_JOB,
    capture::STREAM_TO_URL,
    destinations::CREATE_FOLDER_DESTINATION,
];

pub fn catalog() -> &'static [ToolSpec] {
    CATALOG
}

pub fn spec(name: &str) -> Option<&'static ToolSpec> {
    CATALOG.iter().find(|spec| spec.name == name)
}

/// The catalog as JSON: `[{name, title, kind, risk, needs_session,
/// needs_vision, description, input_schema}]`.
pub fn catalog_json() -> Value {
    Value::Array(CATALOG.iter().map(ToolSpec::description_json).collect())
}

impl ToolSpec {
    /// A small discovery entry for agents which request schemas only as needed.
    pub fn index_json(&self) -> Value {
        json!({
            "name": self.name,
            "title": self.title,
            "kind": self.kind,
            "risk": self.risk,
            "needs_session": self.needs_session,
            "needs_vision": self.needs_vision,
        })
    }

    /// The same complete entry used by the shared catalog and targeted discovery.
    pub fn description_json(&self) -> Value {
        let mut entry = self.index_json();
        entry["description"] = json!(self.description);
        entry["input_schema"] = (self.schema)();
        entry
    }
}

/// One tool call, as agents send it and plan documents store it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCall {
    pub tool: String,
    pub args: Value,
}

/// A problem with a call, attached to the argument it concerns.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Issue {
    /// The argument, as a path such as `region.x`; None for the whole call.
    pub field: Option<String>,
    /// A full sentence telling the reader what to do. Its wording never
    /// decides the exit code; `code` does.
    pub message: String,
    pub severity: Severity,
    pub code: IssueCode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Error,
    Warning,
}

/// The class of an issue, which decides a command-line caller's exit code.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueCode {
    /// The call itself is wrong (exit 2).
    #[default]
    Usage,
    /// A camera, destination, FFmpeg or volume is missing (exit 3).
    Unavailable,
    /// A camera or the session did not answer in time (exit 4).
    Timeout,
    /// This build or camera cannot do it (exit 5).
    Unsupported,
    /// A byte, file or disk budget would be exceeded (exit 6).
    StorageFull,
    /// Running it failed (exit 1).
    OperationFailed,
}

impl IssueCode {
    /// The CLI error code and exit status of this class.
    pub fn exit(self) -> (&'static str, i32) {
        match self {
            Self::Usage => ("usage", 2),
            Self::Unavailable => ("unavailable", 3),
            Self::Timeout => ("timeout", 4),
            Self::Unsupported => ("unsupported", 5),
            Self::StorageFull => ("storage_full", 6),
            Self::OperationFailed => ("operation_failed", 1),
        }
    }

    /// The class the CLI gives `error`, for failures of the commands a tool
    /// sends.
    pub fn of_error(error: &anyhow::Error) -> Self {
        match crate::cli::error_code(error).0 {
            "usage" => Self::Usage,
            "unavailable" => Self::Unavailable,
            "timeout" => Self::Timeout,
            "unsupported" => Self::Unsupported,
            "storage_full" => Self::StorageFull,
            _ => Self::OperationFailed,
        }
    }
}

impl Issue {
    pub fn error(field: Option<&str>, message: impl Into<String>, code: IssueCode) -> Self {
        Self {
            field: field.map(str::to_owned),
            message: message.into(),
            severity: Severity::Error,
            code,
        }
    }

    /// An error in the call itself.
    pub fn usage(field: Option<&str>, message: impl Into<String>) -> Self {
        Self::error(field, message, IssueCode::Usage)
    }

    /// Something the reader should know that does not stop the call.
    pub fn warning(field: Option<&str>, message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Warning,
            ..Self::usage(field, message)
        }
    }

    /// A failed session command as an issue: its message (as a sentence,
    /// with credentials from `known` hidden for `origin`) and its class.
    pub fn failed(error: &anyhow::Error, known: &Value, origin: Origin) -> Self {
        let message = format!("{error:#}");
        let message = match origin {
            Origin::Assistant => redact::for_model_error(&message, known),
            Origin::Cli => redact::for_output_error(&message, known),
        };
        Self::error(None, sentence(&message), IssueCode::of_error(error))
    }

    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }
}

/// `message` with a capital first letter and a final full stop.
pub fn sentence(message: &str) -> String {
    let message = message.trim();
    let mut chars = message.chars();
    let mut out: String = chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default();
    if !out.ends_with(['.', '!', '?']) {
        out.push('.');
    }
    out
}

/// A plan step's identity: assigned when the step is created and never
/// reused, so secrets and edits stay with their step when others move.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct StepId(pub u64);

/// Who called a tool, which decides how its results are redacted and which
/// cameras it may name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// The in-app assistant: results go to an AI provider, and only cameras
    /// in the session's context may be named.
    Assistant,
    /// A person or external agent on the command line.
    Cli,
}

impl Origin {
    /// `value` redacted for this caller.
    pub fn redact(self, value: Value) -> Value {
        match self {
            Origin::Assistant => redact::for_model(value),
            Origin::Cli => redact::for_output(value),
        }
    }

    /// `text` redacted for this caller.
    pub fn redact_text(self, text: &str) -> String {
        match self {
            Origin::Assistant => redact::for_model_text(text),
            Origin::Cli => redact::for_output_text(text),
        }
    }
}

/// Parse a call's arguments into the tool's own type. Every key must be
/// present (null for a default) and match the tool's schema; each problem
/// becomes an issue on its argument.
pub fn parse_args<T: DeserializeOwned>(spec: &ToolSpec, args: &Value) -> Result<T, Vec<Issue>> {
    let schema = (spec.schema)();
    let args = match args {
        // A tool without arguments may be called with null.
        Value::Null
            if schema["properties"]
                .as_object()
                .is_some_and(|p| p.is_empty()) =>
        {
            json!({})
        }
        args => args.clone(),
    };
    let (args, issues) = schema::check(spec.name, &args, &schema);
    if !issues.is_empty() {
        return Err(issues);
    }
    arguments::parse(args).map_err(|(field, error)| {
        vec![Issue::usage(
            field.as_deref(),
            format!(
                "The arguments do not match the {} schema: {error}.",
                spec.name
            ),
        )]
    })
}

/// A camera a call names, resolved against the session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CameraRef {
    /// Connected in the session, by its session ID.
    Connected(String),
    /// Found by the last discovery and not connected, by its discovery ID.
    Discovered(String),
    /// A selector the user typed that is neither connected nor discovered.
    Typed(String),
}

impl CameraRef {
    /// The ID or selector commands address.
    pub fn selector(&self) -> &str {
        match self {
            Self::Connected(id) | Self::Discovered(id) | Self::Typed(id) => id,
        }
    }

    pub fn connected(&self) -> Option<&str> {
        match self {
            Self::Connected(id) => Some(id),
            _ => None,
        }
    }
}

/// Resolve `text` (case-insensitive) to a connected camera, by its ID,
/// address or serial; to a discovered device by its ID; or to either by its
/// exact name. IDs as `redact::for_model` shows them also match, so a model
/// can use the IDs it was given. Ambiguous names list the candidates.
/// `assistant_origin` calls may only name cameras in the context; others
/// may also type any valid selector.
pub fn resolve_camera(
    ctx: &Context,
    field: &str,
    text: &str,
    assistant_origin: bool,
) -> Result<CameraRef, Issue> {
    let text = text.trim();
    if text.is_empty() {
        return Err(Issue::usage(
            Some(field),
            format!("Provide `{field}`: a camera ID or name from list_cameras."),
        ));
    }
    let same = |id: &str| {
        id.eq_ignore_ascii_case(text) || redact::for_model_text(id).eq_ignore_ascii_case(text)
    };
    let ambiguous = |found: &[CameraRef]| {
        let ids: Vec<_> = found.iter().map(CameraRef::selector).collect();
        Issue::usage(
            Some(field),
            format!(
                "More than one camera answers to \"{text}\": {}. Use one camera's ID instead.",
                list(&ids)
            ),
        )
    };
    let snapshot = &ctx.snapshot;
    // IDs first, then names: a name never shadows another camera's ID.
    let by_id: Vec<CameraRef> = snapshot
        .cameras
        .iter()
        .map(|camera| &camera.info)
        .filter(|info| {
            same(&info.id)
                || info
                    .address
                    .as_deref()
                    .is_some_and(|address| same(address) || same(without_query(address)))
                || (!info.serial.is_empty() && info.serial.eq_ignore_ascii_case(text))
        })
        .map(|info| CameraRef::Connected(info.id.clone()))
        .collect();
    let by_id = if by_id.is_empty() {
        snapshot
            .devices
            .iter()
            .filter(|device| {
                connected_device(ctx, device).is_none()
                    && (same(&device.id)
                        || device
                            .address
                            .as_deref()
                            .is_some_and(|address| same(address) || same(without_query(address)))
                        || (!device.serial.is_empty() && device.serial.eq_ignore_ascii_case(text)))
            })
            .map(|device| CameraRef::Discovered(device.id.clone()))
            .collect()
    } else {
        by_id
    };
    match by_id.len() {
        0 => {}
        1 => return Ok(by_id.into_iter().next().expect("one camera")),
        _ => return Err(ambiguous(&by_id)),
    }
    let mut by_name: Vec<CameraRef> = snapshot
        .cameras
        .iter()
        .filter(|camera| camera_name(ctx, &camera.info).eq_ignore_ascii_case(text))
        .map(|camera| CameraRef::Connected(camera.info.id.clone()))
        .chain(
            snapshot
                .devices
                .iter()
                .filter(|device| {
                    device.model.eq_ignore_ascii_case(text)
                        && connected_device(ctx, device).is_none()
                })
                .map(|device| CameraRef::Discovered(device.id.clone())),
        )
        .collect();
    by_name.dedup();
    match by_name.len() {
        0 => {}
        1 => return Ok(by_name.remove(0)),
        _ => return Err(ambiguous(&by_name)),
    }
    if assistant_origin {
        return Err(Issue::error(
            Some(field),
            "Choose a camera from the list, or type its address in this field.",
            IssueCode::Unavailable,
        ));
    }
    match crate::session::validate_selector(text) {
        Ok(()) => Ok(CameraRef::Typed(text.to_owned())),
        Err(error) => Err(Issue::usage(Some(field), sentence(&format!("{error:#}")))),
    }
}

/// The connected camera discovered device `device` already is: the same ID,
/// or the selector a connected webcam or stream was opened with.
pub fn connected_device<'a>(
    ctx: &'a Context,
    device: &CameraInfo,
) -> Option<&'a crate::session::CameraSnapshot> {
    ctx.snapshot.cameras.iter().find(|camera| {
        camera.info.id == device.id
            || camera
                .info
                .address
                .as_deref()
                .is_some_and(|address| without_query(address) == device.id)
    })
}

/// The name people know a camera by. Webcams report a generic model once
/// connected ("AVFOUNDATION input"), so theirs comes from the discovered
/// device their selector names.
pub fn camera_name(ctx: &Context, info: &CameraInfo) -> String {
    if info.transport == Transport::Media
        && let Some(address) = info.address.as_deref()
        && crate::media::is_host_camera(address)
    {
        let selector = without_query(address);
        if let Some(device) = ctx.snapshot.devices.iter().find(|d| d.id == selector) {
            return device.model.clone();
        }
        if let Some((_, name)) = selector.split_once(':')
            && !name.trim().is_empty()
        {
            return name.trim().to_owned();
        }
    }
    info.model.clone()
}

fn without_query(selector: &str) -> &str {
    selector.split_once('?').map_or(selector, |(base, _)| base)
}

/// "a, b and c".
pub fn list(items: &[&str]) -> String {
    match items {
        [] => String::new(),
        [one] => (*one).to_owned(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// `n` with thousands separators: 1,568.
pub fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// A byte count for people, in decimal units as file managers show them:
/// "512 B", "1.8 GB".
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1000.0 && unit + 1 < UNITS.len() {
        size /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

/// Units of duration arguments, largest first, with the spellings each takes.
const DURATION_UNITS: [(&str, &[&str]); 5] = [
    ("d", &["d", "day", "days"]),
    ("h", &["h", "hr", "hrs", "hour", "hours"]),
    ("m", &["m", "min", "mins", "minute", "minutes"]),
    ("s", &["s", "sec", "secs", "second", "seconds"]),
    ("ms", &["ms"]),
];

/// Parse a duration argument: one or more `<decimal><unit>` groups, largest
/// unit first, with optional spaces ("90s", "1h30m", "1 hour", "2.5h").
/// Returns milliseconds (1 ms..10 years) or a message for an issue.
pub fn parse_duration_field(text: &str) -> Result<u64, String> {
    const EXAMPLE: &str = "for example 90s, 15m or 1h30m";
    let text = text.trim().to_ascii_lowercase();
    if text.is_empty() {
        return Err(format!("Enter a duration, {EXAMPLE}."));
    }
    let mut rest = text.as_str();
    let mut total: u64 = 0;
    let mut previous = None;
    while !rest.is_empty() {
        let number_end = rest
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(rest.len());
        let number = &rest[..number_end];
        rest = rest[number_end..].trim_start();
        let unit_end = rest
            .find(|c: char| !c.is_ascii_alphabetic())
            .unwrap_or(rest.len());
        let unit = &rest[..unit_end];
        rest = rest[unit_end..].trim_start();
        if number.is_empty()
            || number.chars().all(|c| c == '.')
            || number.bytes().filter(|byte| *byte == b'.').count() > 1
        {
            return Err(format!(
                "Write the duration as a number and a unit, {EXAMPLE}."
            ));
        }
        if unit.is_empty() {
            return Err(format!("Add a unit, {EXAMPLE}."));
        }
        let Some(rank) = DURATION_UNITS
            .iter()
            .position(|(_, names)| names.contains(&unit))
        else {
            return Err(format!("Use the units ms, s, m, h or d, {EXAMPLE}."));
        };
        if previous.is_some_and(|previous| rank <= previous) {
            return Err(
                "Write each unit once, from largest to smallest, for example 1h30m.".into(),
            );
        }
        previous = Some(rank);
        let millis = if number.chars().all(|c| c == '0' || c == '.') {
            0
        } else {
            crate::scheduling::parse_duration(&format!("{number}{}", DURATION_UNITS[rank].0))
                .map_err(|error| {
                    if error.to_string().contains("whole number of milliseconds") {
                        "Use whole milliseconds, for example 1.5s.".to_owned()
                    } else if error.to_string().contains("10 years")
                        || error.to_string().contains("too large")
                    {
                        "Choose a duration of at most 10 years.".to_owned()
                    } else {
                        format!("Write the duration as a number and a unit, {EXAMPLE}.")
                    }
                })?
        };
        total = total
            .checked_add(millis)
            .filter(|total| *total <= crate::scheduling::MAX_DURATION_MS)
            .ok_or("Choose a duration of at most 10 years.")?;
    }
    if total == 0 {
        return Err("Choose a duration longer than zero.".into());
    }
    Ok(total)
}

/// A duration in the compact form `parse_duration_field` reads back:
/// "1h30m", "90s" as "1m30s", "1s500ms".
pub fn format_duration_field(millis: u64) -> String {
    if millis == 0 {
        return "0s".into();
    }
    let mut rest = millis;
    let mut out = String::new();
    for (size, unit) in [
        (86_400_000, "d"),
        (3_600_000, "h"),
        (60_000, "m"),
        (1000, "s"),
        (1, "ms"),
    ] {
        if rest >= size {
            out.push_str(&format!("{}{unit}", rest / size));
            rest %= size;
        }
    }
    out
}

#[cfg(test)]
mod core_review_tests {
    use super::*;

    #[test]
    fn malformed_zero_duration_components_are_rejected() {
        for text in ["1h0..0s", "1h..s", "0..1s", "1h0.0.0s"] {
            assert!(parse_duration_field(text).is_err(), "{text}");
        }
        assert_eq!(parse_duration_field("1h0.0s").unwrap(), 3_600_000);
    }

    #[test]
    fn discovered_selectors_and_serials_resolve_to_the_device_id() {
        let mut ctx = tests::fixture_context(tests::fixture_snapshot());
        let device = &mut ctx.snapshot.devices[0];
        device.id = "discovered-camera".into();
        device.address = Some("gige:10.11.12.13".into());
        device.serial = "serial-123".into();
        for selector in ["gige:10.11.12.13", "SERIAL-123"] {
            assert_eq!(
                resolve_camera(&ctx, "camera", selector, true).unwrap(),
                CameraRef::Discovered("discovered-camera".into())
            );
        }
    }
}
