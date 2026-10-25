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
    FieldKind::Text,
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
         blocked when a recording or scheduled captures prevent the restore. At most 40 \
         changes and 20 not-applied rows are listed, with truncated true; `capturefab setup \
         diff` shows all. Read-only.",
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
    let suggestion = ctx
        .setups
        .iter()
        .map(|saved| saved.setup.name.as_str())
        .filter(|candidate| crate::hint::is_close(name, candidate))
        .min_by_key(|candidate| crate::hint::edit_distance(name, candidate))
        .map(|close| format!(" Did you mean {close}?"))
        .unwrap_or_default();
    Issue::error(
        Some("name"),
        format!(
            "No saved setup is named {}.{suggestion} Choose one from list_camera_setups.",
            name.trim()
        ),
        IssueCode::Unavailable,
    )
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

fn preview(value: &SetupValue, camera: &CameraSnapshot) -> Preview {
    if !value.at.is_empty() {
        return Preview::Unknown;
    }
    let Some(feature) = camera.features.iter().find(|f| f.name == value.feature) else {
        return Preview::Unknown;
    };
    let Some(live) = feature.value.as_ref() else {
        return Preview::Unknown;
    };
    match (&value.value, live) {
        (Scalar::Bool(saved), Value::Bool(live)) => same(saved == live),
        (Scalar::String(saved), Value::String(live)) => same(saved == live),
        (Scalar::Number(saved), Value::Number(live)) => {
            let (Some(saved), Some(live)) = (saved.as_f64(), live.as_f64()) else {
                return Preview::Unknown;
            };
            let tolerance = match feature.inc.filter(|inc| inc.is_finite() && *inc > 0.0) {
                Some(inc) => inc / 2.0,
                None => 1e-9 * saved.abs().max(live.abs()).max(1.0),
            };
            same((saved - live).abs() <= tolerance)
        }
        _ => Preview::Unknown,
    }
}
fn same(equal: bool) -> Preview {
    if equal {
        Preview::Same
    } else {
        Preview::Change
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
            sentence(&crate::setup::other_model_message(setup, &snapshot.info)),
            IssueCode::Unsupported,
        ));
    }
    for warning in match_warnings(matched, saved, &snapshot.info) {
        checked.warning("name", warning);
    }
    let changes: Vec<&SetupValue> = setup
        .values
        .iter()
        .filter(|value| preview(value, snapshot) != Preview::Same)
        .collect();
    let features: BTreeSet<String> = changes.iter().map(|v| v.feature.clone()).collect();
    let disruptive: Vec<String> = features
        .iter()
        .filter(|feature| !risk::image_quality(feature))
        .cloned()
        .collect();
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
    if !disruptive.is_empty() {
        // Each such restore needs its own approval: a grant for one setup
        // never covers another that changes how the camera triggers or
        // signals other equipment.
        checked.risk = checked.risk.max(Risk::HumanOnly);
        checked.reasons.push(format!(
            "May change {}, outside the image-quality allowlist.",
            list(&disruptive.iter().map(String::as_str).collect::<Vec<_>>())
        ));
    }
    if !checked.ready() {
        return;
    }
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
    checked.facts.push((
        "Changes".into(),
        if changes.is_empty() {
            "None: the camera already reports every saved value.".into()
        } else {
            format!(
                "{} of {} values may change; the camera compares them again before writing.",
                grouped(changes.len() as u64),
                grouped(setup.values.len() as u64)
            )
        },
    ));
    for value in changes.iter().take(MAX_FACTS) {
        checked
            .facts
            .push((label(value), format!("→ {}", value.value.text())));
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
        confirmed_disruptive: Some(disruptive),
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
    checked.ops.push(Op::ApplySetup {
        camera,
        command,
        changes: changes.len(),
        features: features.into_iter().collect(),
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
                if confirmed == &["TriggerMode"]
        ));
        assert!(checked.facts.iter().any(|(name, value)| name
            == "TriggerMode [TriggerSelector=FrameStart]"
            && value == "→ On"));
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
