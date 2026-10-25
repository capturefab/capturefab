//! Saved camera setups for agents: list them, preview one against a camera
//! and restore one as a single undoable camera change.
//!
//! Saving and deleting setups stay with the user (`capturefab setup`, the
//! desktop's setup menu): no tool writes the setup store. A restore step
//! names the setup and pins its content digest instead of carrying the
//! document, so plans stay small and a setup edited after review is refused.
//! Checks read the setups `Context::gather` loaded and the camera's last
//! reported values; the camera worker compares them again before any write
//! and refuses a disruptive change the reviewed step did not name.
use super::{
    Checked, FieldKind, FieldSpec, Issue, IssueCode, Op, Origin, PlanState, Risk, ToolCall,
    ToolKind, ToolSpec,
    context::{Context, SavedSetup, ToolHost},
    grouped, list, parse_args, risk, schema, sentence,
};
use crate::{
    session::{CameraSnapshot, SessionCommand},
    setup::{Match, Scalar, SetupValue},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeSet;

/// Change rows a summary keeps; `capturefab setup diff` shows all of them.
pub const MAX_CHANGES: usize = 40;
/// Not-applied rows a summary keeps.
pub const MAX_NOT_APPLIED: usize = 20;
/// Changes a step's facts name one by one.
const MAX_FACTS: usize = 12;

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
const NAME_FIELD: FieldSpec = field(
    "name",
    "Setup",
    FieldKind::Setup,
    false,
    "A saved setup's name, from list_camera_setups.",
);
const OTHER_MODEL_FIELD: FieldSpec = field(
    "allow_other_model",
    "Allow another model",
    FieldKind::Bool,
    false,
    "Use a setup saved from another camera model where both have the values.",
);
const SETUP_FIELDS: &[FieldSpec] = &[
    field(
        "camera",
        "Camera",
        FieldKind::Camera,
        false,
        "The connected camera to compare or restore.",
    ),
    NAME_FIELD,
    OTHER_MODEL_FIELD,
];

pub const LIST_CAMERA_SETUPS: ToolSpec = ToolSpec {
    name: "list_camera_setups",
    title: "List camera setups",
    kind: ToolKind::Read,
    risk: Risk::Observe,
    needs_session: false,
    needs_vision: false,
    description: "List the camera setups the user saved: name, the camera model it was saved \
         from, when, how many values it holds, its note and whether it turns on auto exposure. \
         camera (a connected camera, or null) adds how each setup matches it: same_camera, \
         same_model, other_model (restoring needs allow_other_model) or unknown_source. Use \
         preview_camera_setup to see what restoring one would change. Saving and deleting \
         setups is left to the user; no tool does it. Works without a session. Read-only.",
    schema: list_schema,
    fields: &[field(
        "camera",
        "Camera",
        FieldKind::Camera,
        true,
        "Show how each setup matches this connected camera; empty lists them only.",
    )],
};

pub const PREVIEW_CAMERA_SETUP: ToolSpec = ToolSpec {
    name: "preview_camera_setup",
    title: "Preview camera setup",
    kind: ToolKind::Read,
    risk: Risk::Observe,
    needs_session: true,
    needs_vision: false,
    description: "Compare a saved setup with a connected camera without changing it. Returns \
         the values a restore would change (feature, saved, current, risk reversible or \
         disruptive, stream_locked), how many already match, values the camera cannot take \
         (not_applied with a reason), the model match and warnings, restarts_stream, and \
         blocked when a recording or scheduled captures prevent the restore, and approval \
         (reversible or human_only, with approval_reason): what apply_camera_setup would \
         need. At most 40 changes and 20 not-applied rows are listed, with truncated true; \
         `capturefab setup diff` shows all. Read-only.",
    schema: setup_schema,
    fields: SETUP_FIELDS,
};

pub const APPLY_CAMERA_SETUP: ToolSpec = ToolSpec {
    name: "apply_camera_setup",
    title: "Restore camera setup",
    kind: ToolKind::Configure,
    risk: Risk::Reversible,
    needs_session: true,
    needs_vision: false,
    description: "Restore a saved setup on a connected camera as one camera change with one \
         undo token; a failure restores every value. The step pins the setup's content, so a \
         setup edited after review is refused. A setup that changes only image-quality values \
         is reversible; one that changes other values, such as trigger, line or counter \
         settings, needs a person's approval every time, and the camera refuses a disruptive \
         change the reviewed step did not list. allow_other_model restores a setup saved from \
         another model where both have the values. A restore that must restart live view is \
         refused while a recording or scheduled captures run. Preview with \
         preview_camera_setup first. Saving and deleting setups is left to the user.",
    schema: setup_schema,
    fields: SETUP_FIELDS,
};

fn list_schema() -> Value {
    schema::object(vec![(
        "camera",
        schema::nullable(schema::string(
            "A connected camera ID or name to match the setups against; null lists them only.",
        )),
    )])
}
fn setup_schema() -> Value {
    schema::object(vec![
        (
            "camera",
            schema::string("A connected camera ID or exact friendly name."),
        ),
        (
            "name",
            schema::string("A saved setup's name, from list_camera_setups."),
        ),
        (
            "allow_other_model",
            schema::boolean("Use a setup saved from another camera model; false refuses one."),
        ),
    ])
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListArgs {
    camera: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SetupArgs {
    camera: String,
    name: String,
    allow_other_model: bool,
}

/// The saved setup `name` names, ignoring case as the store does.
fn find<'a>(ctx: &'a Context, name: &str) -> Option<&'a SavedSetup> {
    let name = name.trim();
    ctx.setups
        .iter()
        .find(|saved| saved.setup.name.eq_ignore_ascii_case(name))
}

fn not_found(ctx: &Context, name: &str) -> Issue {
    let name = name.trim();
    let message = if let Some(problem) = &ctx.setups_problem {
        format!("No readable setup is named {name}. {problem}")
    } else if ctx.setups.is_empty() {
        format!(
            "No saved setup is named {name}: no setups are saved. The user saves one with capturefab setup save NAME."
        )
    } else {
        let suggestion = ctx
            .setups
            .iter()
            .map(|saved| saved.setup.name.as_str())
            .filter(|candidate| crate::hint::is_close(name, candidate))
            .min_by_key(|candidate| crate::hint::edit_distance(name, candidate))
            .map(|close| format!(" Did you mean {close}?"))
            .unwrap_or_default();
        format!("No saved setup is named {name}.{suggestion} Choose one from list_camera_setups.")
    };
    Issue::error(Some("name"), message, IssueCode::Unavailable)
}

/// Warnings for a match, without the serial number another camera has.
fn match_warnings(
    matched: Match,
    saved: &SavedSetup,
    live: &crate::types::CameraInfo,
) -> Vec<String> {
    match matched {
        Match::SameModel => vec![format!(
            "{} was saved from another {}.",
            saved.setup.name,
            saved.setup.camera.label()
        )],
        Match::UnknownSource => crate::setup::matching(&saved.setup.camera, live).1,
        Match::SameCamera | Match::OtherModel => Vec::new(),
    }
}

pub(crate) fn preflight(ctx: &Context, call: &ToolCall, origin: Origin) -> Result<(), Vec<Issue>> {
    match call.tool.as_str() {
        "list_camera_setups" => {
            let args: ListArgs = parse_args(&LIST_CAMERA_SETUPS, &call.args)?;
            if let Some(camera) = args.camera.as_deref() {
                super::read::connected_camera(ctx, "camera", camera, origin)
                    .map_err(|issue| vec![issue])?;
            }
            Ok(())
        }
        _ => {
            let args: SetupArgs = parse_args(&PREVIEW_CAMERA_SETUP, &call.args)?;
            super::read::connected_camera(ctx, "camera", &args.camera, origin)
                .map_err(|issue| vec![issue])?;
            find(ctx, &args.name).ok_or_else(|| vec![not_found(ctx, &args.name)])?;
            Ok(())
        }
    }
}

/// Run `list_camera_setups` or `preview_camera_setup`.
pub(crate) fn read(
    host: &dyn ToolHost,
    ctx: &Context,
    call: &ToolCall,
    origin: Origin,
) -> Result<Value, Vec<Issue>> {
    preflight(ctx, call, origin)?;
    if call.tool == "list_camera_setups" {
        let args: ListArgs = parse_args(&LIST_CAMERA_SETUPS, &call.args)?;
        let camera = match args.camera.as_deref() {
            Some(camera) => Some(
                super::read::connected_camera(ctx, "camera", camera, origin)
                    .map_err(|issue| vec![issue])?,
            ),
            None => None,
        };
        return Ok(list_setups(ctx, camera));
    }
    let args: SetupArgs = parse_args(&PREVIEW_CAMERA_SETUP, &call.args)?;
    let camera = super::read::connected_camera(ctx, "camera", &args.camera, origin)
        .map_err(|issue| vec![issue])?;
    let saved = find(ctx, &args.name).ok_or_else(|| vec![not_found(ctx, &args.name)])?;
    let report = host
        .execute(
            Some(&camera.info.id),
            SessionCommand::SetupDiff {
                name: Some(saved.setup.name.clone()),
                setup: None,
                allow_other_model: args.allow_other_model,
            },
        )
        .map_err(|error| vec![Issue::failed(&error, &call.args, origin)])?;
    let mut summary = summary(report);
    // What apply_camera_setup's check would ask for, from the same
    // comparison, so preview and restore agree.
    let assessed = assess(&saved.setup, camera);
    summary["approval"] = json!(assessed.risk());
    if let Some(reason) = assessed.reasons().into_iter().next() {
        summary["approval_reason"] = json!(reason);
    }
    // The diff names the setup the store holds now; say so when it is no
    // longer the one this context loaded.
    if summary["digest"].as_str() != Some(saved.digest.as_str()) {
        summary["note"] = json!("The setup changed while it was compared; list the setups again.");
    }
    Ok(summary)
}

fn list_setups(ctx: &Context, camera: Option<&CameraSnapshot>) -> Value {
    let setups: Vec<Value> = ctx
        .setups
        .iter()
        .map(|saved| {
            let setup = &saved.setup;
            let mut entry = json!({
                "name": setup.name,
                "saved_from": setup.camera.label(),
                "transport": setup.camera.transport,
                "saved_at": setup.saved_at,
                "values": setup.values.len(),
                "auto_exposure": setup.auto.is_some(),
            });
            if let Some(note) = &setup.note {
                entry["note"] = json!(note);
            }
            if let Some(camera) = camera {
                let (matched, _) = crate::setup::matching(&setup.camera, &camera.info);
                entry["match"] = json!(matched.as_str());
                let warnings = match_warnings(matched, saved, &camera.info);
                if !warnings.is_empty() {
                    entry["warnings"] = json!(warnings);
                }
            }
            entry
        })
        .collect();
    let mut result = json!({
        "count": setups.len(),
        "setups": setups,
        "note": "Saving and deleting setups is left to the user: capturefab setup save or the desktop app.",
    });
    if let Some(camera) = camera {
        result["camera"] = json!(camera.info.id);
    }
    if let Some(problem) = &ctx.setups_problem {
        result["warnings"] = json!([problem]);
    }
    result
}

/// A worker's `setup_diff` or `apply_setup` reply, bounded for tool output:
/// counts, match and warnings, the first changes and not-applied rows, and
/// `truncated` when rows were left out. Matching rows are counted only, and
/// the other camera's serial number is never repeated.
pub(crate) fn summary(report: Value) -> Value {
    let mut out = serde_json::Map::new();
    for key in [
        "setup",
        "digest",
        "match",
        "unchanged",
        "risk",
        "restarts_stream",
        "blocked",
        "applied",
        "auto",
        "restarted_stream",
        "camera_change",
        "before",
        "after",
    ] {
        if let Some(value) = report.get(key) {
            out.insert(key.into(), value.clone());
        }
    }
    let camera = &report["camera"];
    if camera.is_object() {
        out.insert(
            "camera".into(),
            json!({"id":camera["id"],"vendor":camera["vendor"],"model":camera["model"]}),
        );
    }
    let warnings = if report["match"] == "same_model" {
        vec![json!(format!(
            "{} was saved from another camera of this model.",
            report["setup"].as_str().unwrap_or("This setup")
        ))]
    } else {
        report["warnings"].as_array().cloned().unwrap_or_default()
    };
    out.insert("warnings".into(), Value::Array(warnings));
    let mut truncated = false;
    for (key, count, limit) in [
        ("changes", "change_count", MAX_CHANGES),
        ("not_applied", "not_applied_count", MAX_NOT_APPLIED),
        ("before", "", MAX_CHANGES),
        ("after", "", MAX_CHANGES),
    ] {
        let rows = match out.get(key).or_else(|| report.get(key)) {
            Some(Value::Array(rows)) => rows.clone(),
            _ => continue,
        };
        if !count.is_empty() {
            out.insert(count.into(), json!(rows.len()));
        }
        truncated |= rows.len() > limit;
        out.insert(
            key.into(),
            Value::Array(rows.into_iter().take(limit).collect()),
        );
    }
    out.insert("truncated".into(), json!(truncated));
    super::read::bound(Value::Object(out))
}

/// How a saved value compares with the camera's last reported value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Preview {
    Same,
    Change,
    /// A bank the snapshot does not show, a feature it does not list, or a
    /// value of another type: the worker decides, so count it as a change.
    Unknown,
}

/// The text of a reported value, as a fact shows it.
fn text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// Compare `value` with the snapshot, with the current value's text when
/// the snapshot reports it. A banked value compares when every bank's
/// selector currently reports that bank, since the snapshot reads features
/// at the selected bank; other banks stay unknown.
fn preview(value: &SetupValue, camera: &CameraSnapshot) -> (Preview, Option<String>) {
    let reported = |name: &str| {
        camera
            .features
            .iter()
            .find(|f| f.name == name)
            .and_then(|f| f.value.as_ref().map(|value| (f, value)))
    };
    let selected = value.at.iter().all(|bank| {
        reported(&bank.selector).is_some_and(|(_, selected)| text(selected) == bank.value)
    });
    if !selected {
        return (Preview::Unknown, None);
    }
    let Some((feature, live)) = reported(&value.feature) else {
        return (Preview::Unknown, None);
    };
    let current = Some(text(live));
    let preview = match (&value.value, live) {
        (Scalar::Bool(saved), Value::Bool(live)) => same(saved == live),
        (Scalar::String(saved), Value::String(live)) => same(saved == live),
        (Scalar::Number(saved), Value::Number(live)) => {
            let (Some(saved), Some(live)) = (saved.as_f64(), live.as_f64()) else {
                return (Preview::Unknown, current);
            };
            let tolerance = match feature.inc.filter(|inc| inc.is_finite() && *inc > 0.0) {
                Some(inc) => inc / 2.0,
                None => 1e-9 * saved.abs().max(live.abs()).max(1.0),
            };
            same((saved - live).abs() <= tolerance)
        }
        _ => Preview::Unknown,
    };
    (preview, current)
}
fn same(equal: bool) -> Preview {
    if equal {
        Preview::Same
    } else {
        Preview::Change
    }
}

/// A setup compared with a camera's last reading: the values that differ
/// and those the reading cannot answer. The restore check and the
/// preview's `approval` both use it, so they agree.
struct Assessed<'a> {
    /// Values that may change, in setup order, with the current value
    /// when the snapshot reports it.
    rows: Vec<(&'a SetupValue, Preview, Option<String>)>,
}

fn assess<'a>(setup: &'a crate::setup::Setup, camera: &CameraSnapshot) -> Assessed<'a> {
    Assessed {
        rows: setup
            .values
            .iter()
            .filter_map(|value| {
                let (preview, current) = preview(value, camera);
                (preview != Preview::Same).then_some((value, preview, current))
            })
            .collect(),
    }
}

impl Assessed<'_> {
    fn features(&self) -> BTreeSet<String> {
        self.rows.iter().map(|(v, ..)| v.feature.clone()).collect()
    }
    /// Features outside the image-quality allowlist that may change.
    fn disruptive(&self) -> Vec<String> {
        self.features()
            .into_iter()
            .filter(|feature| !risk::image_quality(feature))
            .collect()
    }
    /// The confirmation for each disruptive row, at its exact bank: the
    /// camera refuses a banked change confirmed by its name alone.
    fn confirmations(&self) -> Vec<crate::setup::Confirmed> {
        let mut confirmed: Vec<crate::setup::Confirmed> = Vec::new();
        for (value, ..) in &self.rows {
            if risk::image_quality(&value.feature) {
                continue;
            }
            let row = crate::setup::Confirmed::At(crate::setup::ConfirmedAt {
                feature: value.feature.clone(),
                at: value.at.clone(),
            });
            if !confirmed.contains(&row) {
                confirmed.push(row);
            }
        }
        confirmed
    }
    fn uncompared(&self) -> usize {
        self.rows
            .iter()
            .filter(|(_, preview, _)| *preview == Preview::Unknown)
            .count()
    }
    fn risk(&self) -> Risk {
        if self.disruptive().is_empty() {
            Risk::Reversible
        } else {
            Risk::HumanOnly
        }
    }
    /// Why a person must approve: the values outside the allowlist that
    /// differ, then those the last reading could not compare.
    fn reasons(&self) -> Vec<String> {
        let outside = |preview: Preview| {
            let mut labels = Vec::new();
            for (value, row, _) in &self.rows {
                if *row == preview && !risk::image_quality(&value.feature) {
                    let label = if preview == Preview::Change {
                        value.feature.clone()
                    } else {
                        label(value)
                    };
                    if !labels.contains(&label) {
                        labels.push(label);
                    }
                }
            }
            labels
        };
        let named = |labels: Vec<String>| {
            let shown: Vec<&str> = labels.iter().take(3).map(String::as_str).collect();
            match labels.len().saturating_sub(shown.len()) {
                0 => list(&shown),
                more => format!("{} and {} more", shown.join(", "), grouped(more as u64)),
            }
        };
        let mut reasons = Vec::new();
        let changed = outside(Preview::Change);
        if !changed.is_empty() {
            reasons.push(format!(
                "Changes {}, outside the image-quality allowlist.",
                named(changed)
            ));
        }
        let unknown = outside(Preview::Unknown);
        if !unknown.is_empty() {
            reasons.push(format!(
                "Could not compare {} from the last reading; preview_camera_setup shows the exact difference.",
                named(unknown)
            ));
        }
        reasons
    }
}

fn label(value: &SetupValue) -> String {
    if value.at.is_empty() {
        return value.feature.clone();
    }
    let banks: Vec<String> = value
        .at
        .iter()
        .map(|bank| format!("{}={}", bank.selector, bank.value))
        .collect();
    format!("{} [{}]", value.feature, banks.join(", "))
}

/// Check `apply_camera_setup`.
pub(crate) fn check(ctx: &Context, plan: &mut PlanState, checked: &mut Checked) {
    let args = match parse_args::<SetupArgs>(&APPLY_CAMERA_SETUP, &checked.call.args) {
        Ok(args) => args,
        Err(issues) => {
            checked.issues.extend(issues);
            return;
        }
    };
    let Some((camera, snapshot)) = super::control::connected(ctx, plan, checked, &args.camera)
    else {
        return;
    };
    let Some(snapshot) = snapshot else {
        checked.error(
            "camera",
            "Read this camera's features after connecting it, then add the restore step.",
        );
        return;
    };
    let Some(saved) = find(ctx, &args.name) else {
        checked.issue(not_found(ctx, &args.name));
        return;
    };
    let setup = &saved.setup;
    checked.call.args["name"] = json!(setup.name);
    let (matched, _) = crate::setup::matching(&setup.camera, &snapshot.info);
    if matched == Match::OtherModel && !args.allow_other_model {
        checked.issue(Issue::error(
            Some("allow_other_model"),
            crate::setup::other_model_message(setup, &snapshot.info),
            IssueCode::Unsupported,
        ));
    }
    for warning in match_warnings(matched, saved, &snapshot.info) {
        checked.warning("name", warning);
    }
    let assessed = assess(setup, snapshot);
    let features = assessed.features();
    let locked: Vec<&str> = features
        .iter()
        .map(String::as_str)
        .filter(|feature| crate::genicam::stream_locked(feature))
        .collect();
    let selector = camera.selector().to_owned();
    if plan.is_streaming(&selector) && !locked.is_empty() {
        let mut holders = Vec::new();
        if plan.is_forwarding(&selector) {
            holders.push("the recording or stream");
        }
        if plan.active_jobs(&selector) > 0 {
            holders.push("the scheduled captures");
        }
        if holders.is_empty() {
            checked.warning(
                "camera",
                format!(
                    "Restoring restarts live view once to change {}.",
                    list(&locked)
                ),
            );
        } else {
            checked.issue(Issue::error(
                Some("camera"),
                format!(
                    "Stop {} first: restoring {} changes {}, which needs a live-view restart.",
                    list(&holders),
                    setup.name,
                    list(&locked)
                ),
                IssueCode::Unavailable,
            ));
        }
    }
    let auto_on = snapshot
        .auto
        .as_ref()
        .is_some_and(|auto| auto.state != "off");
    if setup.auto.is_some() {
        checked.warning("name", "Restoring turns on Capturefab's auto exposure.");
    } else if auto_on {
        checked.warning("name", "Restoring turns auto exposure off.");
    }
    // A restore that cannot run is refused with its own error before any
    // approval is asked for.
    if !checked.ready() {
        return;
    }
    if assessed.risk() == Risk::HumanOnly {
        // Each such restore needs its own approval: a grant for one setup
        // never covers another that changes how the camera triggers or
        // signals other equipment.
        checked.risk = checked.risk.max(Risk::HumanOnly);
        checked.reasons.extend(assessed.reasons());
    }
    let changes = &assessed.rows;
    checked.facts.push((
        "Setup".into(),
        format!(
            "{}: {} values saved from {} at {}",
            setup.name,
            grouped(setup.values.len() as u64),
            setup.camera.label(),
            setup.saved_at
        ),
    ));
    checked
        .facts
        .push(("Match".into(), matched.as_str().replace('_', " ")));
    let uncompared = assessed.uncompared();
    let differ = changes.len() - uncompared;
    checked.facts.push((
        "Changes".into(),
        if changes.is_empty() {
            "None: the camera already reports every saved value.".into()
        } else {
            format!(
                "{} of {} values differ from the last reading; the camera compares them again before writing.",
                grouped(differ as u64),
                grouped(setup.values.len() as u64)
            )
        },
    ));
    if uncompared > 0 {
        checked.facts.push((
            "Not compared".into(),
            format!(
                "{} values at other selector banks or not in the last reading may also change.",
                grouped(uncompared as u64)
            ),
        ));
    }
    for (value, _, current) in changes.iter().take(MAX_FACTS) {
        checked.facts.push((
            label(value),
            format!(
                "{} → {}",
                current.as_deref().unwrap_or("not compared"),
                value.value.text()
            ),
        ));
    }
    if changes.len() > MAX_FACTS {
        checked.facts.push((
            "More".into(),
            format!(
                "{} more; preview_camera_setup lists them.",
                grouped((changes.len() - MAX_FACTS) as u64)
            ),
        ));
    }
    let command = SessionCommand::ApplySetup {
        name: Some(setup.name.clone()),
        setup: None,
        digest: Some(saved.digest.clone()),
        allow_other_model: args.allow_other_model,
        strict: false,
        confirmed_disruptive: Some(assessed.confirmations()),
    };
    if let Err(error) = command.validate() {
        checked.error("name", sentence(&format!("{error:#}")));
        return;
    }
    checked.summary = format!("Restore setup {} on {}.", setup.name, selector);
    plan.changed_features
        .entry(selector)
        .or_default()
        .extend(features.iter().cloned());
    let banked = setup
        .values
        .iter()
        .filter(|value| !value.at.is_empty() && features.contains(&value.feature))
        .map(|value| value.feature.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    checked.ops.push(Op::ApplySetup {
        camera,
        command,
        changes: changes.len(),
        features: features.into_iter().collect(),
        banked,
        auto: setup.auto.is_some(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        setup::{Bank, SavedAuto, SavedCamera, Setup},
        tools::{
            StepId,
            context::SavedSetup,
            tests::{FakeHost, fixture_context, fixture_snapshot},
        },
    };

    fn number(value: f64) -> Scalar {
        Scalar::Number(serde_json::Number::from_f64(value).unwrap())
    }
    fn value(feature: &str, value: Scalar) -> SetupValue {
        SetupValue {
            feature: feature.into(),
            value,
            at: Vec::new(),
        }
    }
    /// A setup saved from fixture camera sim:0.
    fn setup(name: &str, values: Vec<SetupValue>) -> Setup {
        let info = crate::transport::simulator::info_named("sim:0");
        Setup {
            version: 1,
            name: name.into(),
            saved_at: "2026-10-18T10:00:00Z".into(),
            note: None,
            camera: SavedCamera::from_info(&info),
            auto: None,
            values,
            skipped: Vec::new(),
        }
    }
    fn context(setups: Vec<Setup>) -> Context {
        let mut ctx = fixture_context(fixture_snapshot());
        // sim:0 neither records nor runs schedules here.
        let sim0 = &mut ctx.snapshot.cameras[0];
        sim0.jobs.clear();
        sim0.auto = None;
        ctx.setups = setups.into_iter().map(SavedSetup::new).collect();
        ctx
    }
    fn call(tool: &str, args: Value) -> ToolCall {
        ToolCall {
            tool: tool.into(),
            args,
        }
    }
    fn apply(ctx: &Context, name: &str) -> Checked {
        crate::tools::check(
            ctx,
            &mut PlanState::new(ctx),
            StepId(1),
            &call(
                "apply_camera_setup",
                json!({"camera":"sim:0","name":name,"allow_other_model":false}),
            ),
            &|_, _| false,
        )
    }
    fn image_only() -> Setup {
        setup(
            "bench",
            vec![
                value("ExposureTime", number(5000.0)),
                value("Gain", number(0.0)),
                value("PixelFormat", Scalar::String("Mono8".into())),
            ],
        )
    }
    fn with_trigger() -> Setup {
        let mut setup = image_only();
        setup.name = "line".into();
        setup.values.push(SetupValue {
            feature: "TriggerMode".into(),
            value: Scalar::String("On".into()),
            at: vec![Bank {
                selector: "TriggerSelector".into(),
                value: "FrameStart".into(),
            }],
        });
        setup
    }

    #[test]
    fn listing_shows_setups_and_their_match_without_serials() {
        let mut other = image_only();
        other.name = "other".into();
        other.camera.model = "Another model".into();
        let mut same_model = image_only();
        same_model.name = "twin".into();
        same_model.camera.serial = Some("SERIAL-OF-ANOTHER".into());
        same_model.note = Some("bench lighting".into());
        same_model.auto = Some(SavedAuto { balance: 0.5 });
        let ctx = context(vec![image_only(), other, same_model]);
        let host = FakeHost::new(ctx.snapshot.clone());
        let listed = read(
            &host,
            &ctx,
            &call("list_camera_setups", json!({"camera":"sim:0"})),
            Origin::Assistant,
        )
        .unwrap();
        assert_eq!(listed["count"], 3);
        let names: Vec<_> = listed["setups"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| (s["name"].clone(), s["match"].clone()))
            .collect();
        assert_eq!(
            names,
            [
                (json!("bench"), json!("same_camera")),
                (json!("other"), json!("other_model")),
                (json!("twin"), json!("same_model")),
            ]
        );
        assert_eq!(listed["setups"][0]["values"], 3);
        assert_eq!(listed["setups"][2]["note"], "bench lighting");
        assert_eq!(listed["setups"][2]["auto_exposure"], true);
        assert!(
            !listed.to_string().contains("SERIAL-OF-ANOTHER"),
            "{listed}"
        );
        // Without a camera it only lists, and never sends a command.
        let plain = read(
            &host,
            &ctx,
            &call("list_camera_setups", json!({"camera":null})),
            Origin::Cli,
        )
        .unwrap();
        assert!(plain["setups"][0].get("match").is_none());
        assert!(host.calls().is_empty());
        let error = read(
            &host,
            &ctx,
            &call("list_camera_setups", json!({"camera":"sim:9"})),
            Origin::Assistant,
        )
        .unwrap_err();
        assert_eq!(error[0].code, IssueCode::Unavailable);
    }

    #[test]
    fn an_image_quality_restore_is_reversible_and_pins_name_and_digest() {
        let ctx = context(vec![image_only()]);
        let checked = apply(&ctx, "BENCH");
        assert!(checked.ready(), "{checked:?}");
        assert_eq!(checked.risk, Risk::Reversible);
        assert_eq!(checked.call.args["name"], "bench");
        let [
            Op::ApplySetup {
                command,
                changes,
                features,
                ..
            },
        ] = checked.ops.as_slice()
        else {
            panic!("{checked:?}");
        };
        // ExposureTime differs; Gain and PixelFormat already match.
        assert_eq!(*changes, 1);
        assert_eq!(features, &["ExposureTime"]);
        let SessionCommand::ApplySetup {
            name,
            setup,
            digest,
            confirmed_disruptive,
            ..
        } = command
        else {
            panic!("{command:?}");
        };
        assert_eq!(name.as_deref(), Some("bench"));
        assert!(setup.is_none(), "plans carry the name, not the document");
        assert_eq!(
            digest.as_deref(),
            Some(crate::setup::digest(&image_only()).as_str())
        );
        assert_eq!(confirmed_disruptive.as_deref(), Some(&[][..]));
    }

    #[test]
    fn a_restore_with_other_features_needs_a_person_every_time() {
        let ctx = context(vec![with_trigger()]);
        let checked = apply(&ctx, "line");
        assert!(checked.ready(), "{checked:?}");
        assert_eq!(checked.risk, Risk::HumanOnly);
        assert!(
            checked.reasons.iter().any(|r| r.contains("TriggerMode")),
            "{:?}",
            checked.reasons
        );
        let Op::ApplySetup {
            command, changes, ..
        } = &checked.ops[0]
        else {
            panic!();
        };
        // A banked value the snapshot cannot show counts as a change.
        assert_eq!(*changes, 2);
        assert!(matches!(
            command,
            SessionCommand::ApplySetup { confirmed_disruptive: Some(confirmed), .. }
                if confirmed == &[crate::setup::Confirmed::At(crate::setup::ConfirmedAt {
                    feature: "TriggerMode".into(),
                    at: vec![crate::setup::Bank {
                        selector: "TriggerSelector".into(),
                        value: "FrameStart".into(),
                    }],
                })]
        ));
        // The fixture lists no TriggerSelector, so the bank is not compared.
        assert!(
            checked.reasons.iter().any(|r| r
                == "Could not compare TriggerMode [TriggerSelector=FrameStart] from the last reading; preview_camera_setup shows the exact difference."),
            "{:?}",
            checked.reasons
        );
        assert!(checked.facts.iter().any(|(name, value)| name
            == "TriggerMode [TriggerSelector=FrameStart]"
            && value == "not compared → On"));
        assert!(
            checked
                .facts
                .iter()
                .any(|(name, value)| name == "Not compared" && value.starts_with("1 values")),
            "{:?}",
            checked.facts
        );
        // Facts show what each value changes from.
        assert!(
            checked
                .facts
                .iter()
                .any(|(name, value)| name == "ExposureTime" && value == "10000.0 → 5000.0"),
            "{:?}",
            checked.facts
        );
    }

    /// sim:0 as a GenICam camera reports its trigger: TriggerMode at the
    /// selected TriggerSelector bank.
    fn with_trigger_features(ctx: &mut Context, selected: &str, mode: &str) {
        let features = std::sync::Arc::make_mut(&mut ctx.snapshot.cameras[0].features);
        let mut features = features.to_vec();
        features.retain(|f| f.name != "TriggerSelector" && f.name != "TriggerMode");
        features.push(crate::tools::tests::feature(
            "TriggerSelector",
            "Enumeration",
            json!(selected),
        ));
        features.push(crate::tools::tests::feature(
            "TriggerMode",
            "Enumeration",
            json!(mode),
        ));
        ctx.snapshot.cameras[0].features = features.into();
    }
    fn at_bank(feature: &str, value: &str, selector: &str, bank: &str) -> SetupValue {
        SetupValue {
            feature: feature.into(),
            value: Scalar::String(value.into()),
            at: vec![Bank {
                selector: selector.into(),
                value: bank.into(),
            }],
        }
    }

    #[test]
    fn a_fresh_setup_with_its_trigger_on_the_selected_bank_is_reversible() {
        // Saved just now: every value matches, including TriggerMode at the
        // FrameStart bank the camera has selected.
        let mut fresh = setup(
            "line",
            vec![
                value("ExposureTime", number(10000.0)),
                at_bank("TriggerMode", "Off", "TriggerSelector", "FrameStart"),
                value("TriggerSelector", Scalar::String("FrameStart".into())),
            ],
        );
        let mut ctx = context(vec![fresh.clone()]);
        with_trigger_features(&mut ctx, "FrameStart", "Off");
        let checked = apply(&ctx, "line");
        assert!(checked.ready(), "{checked:?}");
        assert_eq!(checked.risk, Risk::Reversible, "{:?}", checked.reasons);
        let Op::ApplySetup {
            changes, banked, ..
        } = &checked.ops[0]
        else {
            panic!();
        };
        assert_eq!(*changes, 0);
        assert!(banked.is_empty());
        // A selected bank that differs is a known change that needs a person.
        with_trigger_features(&mut ctx, "FrameStart", "On");
        let changed = apply(&ctx, "line");
        assert_eq!(changed.risk, Risk::HumanOnly);
        assert_eq!(
            changed.reasons[0],
            "Changes TriggerMode, outside the image-quality allowlist."
        );
        assert!(changed.facts.iter().any(|(name, value)| name
            == "TriggerMode [TriggerSelector=FrameStart]"
            && value == "On → Off"));
        let Op::ApplySetup { banked, .. } = &changed.ops[0] else {
            panic!();
        };
        assert_eq!(banked, &["TriggerMode"]);
        // A bank the camera has not selected cannot be compared.
        fresh.values.push(at_bank(
            "TriggerMode",
            "Off",
            "TriggerSelector",
            "LineStart",
        ));
        let mut ctx = context(vec![fresh]);
        with_trigger_features(&mut ctx, "FrameStart", "Off");
        let other_bank = apply(&ctx, "line");
        assert_eq!(other_bank.risk, Risk::HumanOnly);
        assert_eq!(
            other_bank.reasons[0],
            "Could not compare TriggerMode [TriggerSelector=LineStart] from the last reading; preview_camera_setup shows the exact difference."
        );
    }

    #[test]
    fn preview_reports_the_approval_the_restore_would_need() {
        let report = |setup: &Setup| {
            json!({"setup":setup.name,"digest":crate::setup::digest(setup),
                "match":"same_camera","warnings":[],"changes":[],"unchanged":3,"not_applied":[]})
        };
        let preview_of = |ctx: &Context, setup: &Setup| {
            let host = FakeHost::new(ctx.snapshot.clone()).reply("setup_diff", Ok(report(setup)));
            read(
                &host,
                ctx,
                &call(
                    "preview_camera_setup",
                    json!({"camera":"sim:0","name":setup.name,"allow_other_model":false}),
                ),
                Origin::Assistant,
            )
            .unwrap()
        };
        let ctx = context(vec![image_only()]);
        let reversible = preview_of(&ctx, &image_only());
        assert_eq!(reversible["approval"], "reversible");
        assert!(reversible.get("approval_reason").is_none());
        assert_eq!(apply(&ctx, "bench").risk, Risk::Reversible);
        let ctx = context(vec![with_trigger()]);
        let human = preview_of(&ctx, &with_trigger());
        assert_eq!(human["approval"], "human_only");
        assert!(
            human["approval_reason"]
                .as_str()
                .unwrap()
                .starts_with("Could not compare TriggerMode"),
            "{human}"
        );
        assert_eq!(apply(&ctx, "line").risk, Risk::HumanOnly);
    }

    #[test]
    fn an_unreadable_store_or_no_setups_is_said_instead_of_a_missing_name() {
        let mut ctx = context(Vec::new());
        let none = apply(&ctx, "bench");
        assert_eq!(none.issues[0].code, IssueCode::Unavailable);
        assert!(
            none.issues[0].message.contains(
                "no setups are saved. The user saves one with capturefab setup save NAME."
            ),
            "{:?}",
            none.issues
        );
        let dir = std::env::temp_dir().join(format!(
            "capturefab-tool-setups-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = crate::setup::Store::at(&dir);
        store.save(&image_only(), false).unwrap();
        std::fs::write(dir.join("broken.json"), b"{").unwrap();
        let (setups, problem) = crate::tools::context::setups_in(&store);
        assert_eq!(setups.len(), 1);
        let problem = problem.unwrap();
        assert!(problem.starts_with("broken.json in "), "{problem}");
        ctx.setups = setups.into_iter().map(SavedSetup::new).collect();
        ctx.setups_problem = Some(problem.clone());
        let host = FakeHost::new(ctx.snapshot.clone());
        let listed = read(
            &host,
            &ctx,
            &call("list_camera_setups", json!({"camera":null})),
            Origin::Assistant,
        )
        .unwrap();
        assert_eq!(listed["count"], 1);
        assert_eq!(listed["warnings"], json!([problem]));
        let missing = apply(&ctx, "broken");
        assert!(
            missing.issues[0].message.contains("broken.json in "),
            "{:?}",
            missing.issues
        );
        // A store folder that cannot be listed.
        let file = dir.join("not-a-folder");
        std::fs::write(&file, b"").unwrap();
        let (setups, problem) = crate::tools::context::setups_in(&crate::setup::Store::at(&file));
        assert!(setups.is_empty());
        assert!(problem.unwrap().starts_with("The setup store "));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_setups_other_models_and_busy_restarts_are_refused() {
        let ctx = context(vec![image_only()]);
        let missing = apply(&ctx, "bnech");
        assert_eq!(missing.issues[0].code, IssueCode::Unavailable);
        assert!(
            missing.issues[0].message.contains("Did you mean bench?"),
            "{:?}",
            missing.issues
        );
        assert!(missing.ops.is_empty());

        let mut other = image_only();
        other.camera.model = "Another model".into();
        let ctx = context(vec![other]);
        let refused = apply(&ctx, "bench");
        assert_eq!(refused.issues[0].code, IssueCode::Unsupported);
        assert_eq!(
            refused.issues[0].field.as_deref(),
            Some("allow_other_model")
        );
        // The user's own name, as saved, and no approval for a refused step.
        assert!(
            refused.issues[0]
                .message
                .starts_with("bench was saved from "),
            "{:?}",
            refused.issues
        );
        assert_eq!(refused.risk, Risk::Reversible);
        let mut other_line = with_trigger();
        other_line.camera.model = "Another model".into();
        let refused = apply(&context(vec![other_line]), "line");
        assert_eq!(refused.issues[0].code, IssueCode::Unsupported);
        assert_eq!(refused.risk, Risk::Reversible, "{:?}", refused.reasons);
        let allowed = crate::tools::check(
            &ctx,
            &mut PlanState::new(&ctx),
            StepId(1),
            &call(
                "apply_camera_setup",
                json!({"camera":"sim:0","name":"bench","allow_other_model":true}),
            ),
            &|_, _| false,
        );
        assert!(allowed.ready(), "{allowed:?}");

        // sim:0 streams: a Width change restarts live view, which its
        // running schedule forbids.
        let mut wide = image_only();
        wide.values.push(value("Width", number(1280.0)));
        let mut ctx = context(vec![wide]);
        let restart = apply(&ctx, "bench");
        assert!(restart.ready());
        assert!(
            restart
                .issues
                .iter()
                .any(|i| i.message.contains("restarts live view once"))
        );
        ctx.snapshot.cameras[0].jobs = vec![crate::tools::tests::job(7, "running")];
        let blocked = apply(&ctx, "bench");
        let issue = blocked.issues.iter().find(|i| i.is_error()).unwrap();
        assert_eq!(issue.code, IssueCode::Unavailable);
        assert!(issue.message.contains("scheduled captures"), "{issue:?}");
        assert!(blocked.ops.is_empty());
    }

    #[test]
    fn a_setup_edited_after_approval_is_refused_before_any_command() {
        let ctx = context(vec![image_only()]);
        let steps = [(
            StepId(1),
            call(
                "apply_camera_setup",
                json!({"camera":"sim:0","name":"bench","allow_other_model":false}),
            ),
        )];
        let approved = crate::tools::check_plan(&ctx, &steps, &|_, _| false);
        assert!(approved[0].ready());
        let mut edited = image_only();
        edited.note = Some("edited".into());
        let changed = context(vec![edited]);
        let host = FakeHost::new(changed.snapshot.clone());
        let error = crate::tools::apply(
            &host,
            &crate::tools::EmptySecrets,
            || Ok(changed.clone()),
            &steps,
            Some(&approved),
            |_| {},
            &std::sync::atomic::AtomicBool::new(false),
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("changed since you approved it"),
            "{error}"
        );
        assert!(host.calls().is_empty());
    }

    #[test]
    fn preview_and_restore_results_are_bounded_and_drop_serials() {
        let changes: Vec<Value> = (0..100)
            .map(|i| json!({"feature":format!("F{i}"),"saved":i,"current":0,"risk":"reversible"}))
            .collect();
        let report = json!({
            "setup":"bench","digest":"sha256:00","match":"same_model",
            "warnings":["Saved from another Pattern camera (serial SN-SECRET)."],
            "camera":{"id":"sim:0","vendor":"V","model":"M","serial":"SN-LIVE"},
            "changes":changes,"unchanged":3,"matched":[{"feature":"Gain","value":0}],
            "not_applied":(0..30).map(|i| json!({"feature":format!("N{i}")})).collect::<Vec<_>>(),
            "risk":"reversible","restarts_stream":false,"blocked":null,
        });
        let summary = summary(report);
        assert_eq!(summary["change_count"], 100);
        assert_eq!(summary["changes"].as_array().unwrap().len(), MAX_CHANGES);
        assert_eq!(summary["not_applied_count"], 30);
        assert_eq!(
            summary["not_applied"].as_array().unwrap().len(),
            MAX_NOT_APPLIED
        );
        assert_eq!(summary["truncated"], true);
        assert!(summary.get("matched").is_none());
        let text = summary.to_string();
        assert!(!text.contains("SN-"), "{text}");
        assert!(text.len() <= crate::tools::MAX_OUTPUT_BYTES);
    }

    #[test]
    fn preview_sends_a_named_read_only_diff() {
        let ctx = context(vec![image_only()]);
        let host = FakeHost::new(ctx.snapshot.clone()).reply(
            "setup_diff",
            Ok(
                json!({"setup":"bench","digest":crate::setup::digest(&image_only()),
                "match":"same_camera","warnings":[],"changes":[],"unchanged":3,"not_applied":[]}),
            ),
        );
        let preview = read(
            &host,
            &ctx,
            &call(
                "preview_camera_setup",
                json!({"camera":"sim:0","name":"Bench","allow_other_model":false}),
            ),
            Origin::Assistant,
        )
        .unwrap();
        assert_eq!(preview["unchanged"], 3);
        assert!(preview.get("note").is_none());
        assert!(matches!(
            &host.calls()[..],
            [(Some(camera), SessionCommand::SetupDiff { name: Some(name), setup: None, .. })]
                if camera == "sim:0" && name == "bench"
        ));
    }
}
