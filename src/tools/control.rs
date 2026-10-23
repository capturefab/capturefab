//! Camera control preflight. Every operation names a pinned camera and
//! feature batches are checked in full before any command is built.
use super::{
    CameraRef, Checked, FieldKind, FieldSpec, Issue, IssueCode, Op, Origin, PlanState, Risk,
    ToolKind, ToolSpec, context::Context, parse_args, risk, schema, sentence,
};
use crate::{
    genicam::FeatureInfo,
    session::{ActivityState, CameraSnapshot, FeatureValue, SessionCommand},
    types::Transport,
};
use serde::Deserialize;
use serde_json::{Value, json};

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
const CAMERA_FIELD: FieldSpec = field(
    "camera",
    "Camera",
    FieldKind::Camera,
    false,
    "The camera to control.",
);
const fn tool(
    name: &'static str,
    title: &'static str,
    description: &'static str,
    schema: fn() -> Value,
    fields: &'static [FieldSpec],
) -> ToolSpec {
    ToolSpec {
        name,
        title,
        kind: ToolKind::Control,
        risk: Risk::Reversible,
        needs_session: true,
        needs_vision: false,
        description,
        schema,
        fields,
    }
}

pub const CONNECT_CAMERA: ToolSpec = tool(
    "connect_camera",
    "Connect camera",
    "Connect a camera from list_cameras, or a selector entered by the user. size (WxH, 16..8192 pixels per side) and fps (0.1..240) apply only to host webcams; use set_camera_features for other cameras. Does not start live view. Already connected cameras need no command.",
    connect_schema,
    &[
        CAMERA_FIELD,
        field(
            "size",
            "Size",
            FieldKind::Text,
            true,
            "Webcams only, such as 1920x1080; empty uses the source default.",
        ),
        field(
            "fps",
            "Frame rate",
            FieldKind::Number {
                min: 0.1,
                max: 240.0,
            },
            true,
            "Webcams only; empty uses the source default.",
        ),
    ],
);
pub const START_LIVE_VIEW: ToolSpec = tool(
    "start_live_view",
    "Start live view",
    "Start acquisition on a connected camera. A scanner start can feed and scan pages and needs approval. To connect a camera first, propose connect_camera.",
    camera_schema,
    &[CAMERA_FIELD],
);
pub const STOP_LIVE_VIEW: ToolSpec = tool(
    "stop_live_view",
    "Stop live view",
    "Stop acquisition on a connected camera. Refuses while a recording, stream or scheduled captures are active: stop or cancel them first. A stopped camera can still be inspected without saving a file.",
    camera_schema,
    &[CAMERA_FIELD],
);
pub const SET_CAMERA_FEATURES: ToolSpec = tool(
    "set_camera_features",
    "Set camera settings",
    "Set 1..16 feature values atomically, in the given order, with one undo token. Values and optional expect values are strings. expect checks the current value before any write. Capturefab-managed transport, persistence, registers and commands are refused. A managed imaging setting turns auto exposure off. restart_stream allows Stop, SetMany, Start for stream-locked settings only while no recording or scheduled captures are active.",
    features_schema,
    &[
        CAMERA_FIELD,
        field(
            "values",
            "Settings",
            FieldKind::Features,
            false,
            "1 to 16 feature, value and nullable expected-value entries.",
        ),
        field(
            "restart_stream",
            "Restart live view",
            FieldKind::Bool,
            false,
            "Temporarily stop live view for settings locked while streaming.",
        ),
    ],
);
pub const SET_CAMERA_ROI: ToolSpec = tool(
    "set_camera_roi",
    "Set camera region",
    "Set a sensor region in pixels, rounding dimensions and offsets to the camera's increments and bounds. Empty values keep the current region. full true uses WidthMax and HeightMax and resets offsets. One atomic write resets offsets, sets size, then sets offsets. An idle live view is restarted when needed; stop recordings and scheduled captures first.",
    roi_schema,
    &[
        CAMERA_FIELD,
        field(
            "full",
            "Full sensor",
            FieldKind::Bool,
            false,
            "Restore full sensor dimensions and zero offsets.",
        ),
        field(
            "x",
            "Left",
            FieldKind::Integer {
                min: 0,
                max: i64::MAX,
            },
            true,
            "Horizontal sensor offset in pixels.",
        ),
        field(
            "y",
            "Top",
            FieldKind::Integer {
                min: 0,
                max: i64::MAX,
            },
            true,
            "Vertical sensor offset in pixels.",
        ),
        field(
            "width",
            "Width",
            FieldKind::Integer {
                min: 1,
                max: i64::MAX,
            },
            true,
            "Region width in pixels.",
        ),
        field(
            "height",
            "Height",
            FieldKind::Integer {
                min: 1,
                max: i64::MAX,
            },
            true,
            "Region height in pixels.",
        ),
    ],
);
const AUTO_MODES: &[(&str, &str)] = &[
    ("auto", "Automatic"),
    ("hold", "Hold current values"),
    ("revert", "Restore values before auto"),
];
pub const SET_AUTO_EXPOSURE: ToolSpec = tool(
    "set_auto_exposure",
    "Set auto exposure",
    "Choose auto, hold (turn auto off and keep its current values), or revert (turn auto off and restore the values before auto). balance applies only to auto, from 0 for quality to 1 for frame rate; null uses the existing/default balance. Use wait_for auto_settled before inspecting the result.",
    auto_schema,
    &[
        CAMERA_FIELD,
        field(
            "mode",
            "Mode",
            FieldKind::Choice(AUTO_MODES),
            false,
            "Automatic, hold, or restore pre-auto settings.",
        ),
        field(
            "balance",
            "Balance",
            FieldKind::Number { min: 0.0, max: 1.0 },
            true,
            "0 favors quality; 1 favors frame rate.",
        ),
    ],
);
pub const RUN_CAMERA_COMMAND: ToolSpec = tool(
    "run_camera_command",
    "Run camera command",
    "Execute an existing TriggerSoftware, TimestampLatch or one-shot AutoOnce-style command. AcquisitionStart and AcquisitionStop map to live-view tools and preserve their checks. Other commands, including reset, firmware, transport, files and user sets, are refused. Commands cannot be undone.",
    command_schema,
    &[
        CAMERA_FIELD,
        field(
            "command",
            "Command",
            FieldKind::Text,
            false,
            "An existing allowlisted command, such as TriggerSoftware.",
        ),
    ],
);
pub const UNDO_CAMERA_CHANGE: ToolSpec = tool(
    "undo_camera_change",
    "Undo camera change",
    "Undo a camera change using its camera_change token. token null selects the latest token this caller obtained on that camera, never another participant's latest change. A token belongs to one camera worker lifetime; conflicts are reported without overwriting newer changes.",
    undo_schema,
    &[
        CAMERA_FIELD,
        field(
            "token",
            "Change token",
            FieldKind::Text,
            true,
            "Empty uses this caller's latest change token for the camera.",
        ),
    ],
);

fn camera_property() -> (&'static str, Value) {
    (
        "camera",
        schema::string("A camera ID or exact friendly name."),
    )
}
fn camera_schema() -> Value {
    schema::object(vec![camera_property()])
}
fn connect_schema() -> Value {
    schema::object(vec![
        camera_property(),
        (
            "size",
            schema::nullable(schema::string(
                "Host webcam size WxH, each side 16..8192; null for the source default.",
            )),
        ),
        (
            "fps",
            schema::nullable(schema::number(
                "Host webcam frame rate 0.1..240; null for the source default.",
            )),
        ),
    ])
}
fn features_schema() -> Value {
    schema::object(vec![
        camera_property(),
        (
            "values",
            schema::array(
                schema::object(vec![
                    ("feature", schema::string("The exact feature name.")),
                    (
                        "value",
                        schema::string(
                            "The value in the feature's unit, or an enumeration choice.",
                        ),
                    ),
                    (
                        "expect",
                        schema::nullable(schema::string(
                            "The expected live value; null skips this concurrency check.",
                        )),
                    ),
                ]),
                "1..16 ordered feature values; all are checked before any write.",
            ),
        ),
        (
            "restart_stream",
            schema::boolean("Allow a safe Stop, SetMany, Start for stream-locked features."),
        ),
    ])
}
fn roi_schema() -> Value {
    schema::object(vec![
        camera_property(),
        (
            "full",
            schema::boolean("Use the full sensor and zero offsets."),
        ),
        (
            "x",
            schema::nullable(schema::integer(
                "Left sensor offset in pixels; null keeps the current value.",
            )),
        ),
        (
            "y",
            schema::nullable(schema::integer(
                "Top sensor offset in pixels; null keeps the current value.",
            )),
        ),
        (
            "width",
            schema::nullable(schema::integer(
                "Width in sensor pixels; null keeps the current value.",
            )),
        ),
        (
            "height",
            schema::nullable(schema::integer(
                "Height in sensor pixels; null keeps the current value.",
            )),
        ),
    ])
}
fn auto_schema() -> Value {
    schema::object(vec![
        camera_property(),
        (
            "mode",
            schema::choice(
                &["auto", "hold", "revert"],
                "Choose automatic control, hold current values, or restore pre-auto values.",
            ),
        ),
        (
            "balance",
            schema::nullable(schema::number(
                "0..1, quality to frame rate; only for auto, null keeps the default.",
            )),
        ),
    ])
}
fn command_schema() -> Value {
    schema::object(vec![
        camera_property(),
        (
            "command",
            schema::string("An existing allowlisted camera command."),
        ),
    ])
}
fn undo_schema() -> Value {
    schema::object(vec![
        camera_property(),
        (
            "token",
            schema::nullable(schema::string(
                "A camera_change token, or null for this caller's latest token.",
            )),
        ),
    ])
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CameraArgs {
    camera: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConnectArgs {
    camera: String,
    size: Option<String>,
    fps: Option<f64>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FeaturesArgs {
    camera: String,
    values: Vec<Setting>,
    restart_stream: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Setting {
    feature: String,
    value: String,
    expect: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RoiArgs {
    camera: String,
    full: bool,
    x: Option<i64>,
    y: Option<i64>,
    width: Option<i64>,
    height: Option<i64>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AutoArgs {
    camera: String,
    mode: String,
    balance: Option<f64>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandArgs {
    camera: String,
    command: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UndoArgs {
    camera: String,
    token: Option<String>,
}

pub(crate) fn check(
    ctx: &Context,
    plan: &mut PlanState,
    checked: &mut Checked,
    _secrets_present: &dyn Fn(super::StepId, &str) -> bool,
) {
    match checked.call.tool.as_str() {
        "connect_camera" => parse::<ConnectArgs>(checked, &CONNECT_CAMERA)
            .map(|args| connect(ctx, plan, checked, args)),
        "start_live_view" => parse::<CameraArgs>(checked, &START_LIVE_VIEW)
            .map(|args| live(ctx, plan, checked, &args.camera, true)),
        "stop_live_view" => parse::<CameraArgs>(checked, &STOP_LIVE_VIEW)
            .map(|args| live(ctx, plan, checked, &args.camera, false)),
        "set_camera_features" => parse::<FeaturesArgs>(checked, &SET_CAMERA_FEATURES)
            .map(|args| features(ctx, plan, checked, args)),
        "set_camera_roi" => {
            parse::<RoiArgs>(checked, &SET_CAMERA_ROI).map(|args| roi(ctx, plan, checked, args))
        }
        "set_auto_exposure" => parse::<AutoArgs>(checked, &SET_AUTO_EXPOSURE)
            .map(|args| auto(ctx, plan, checked, args)),
        "run_camera_command" => parse::<CommandArgs>(checked, &RUN_CAMERA_COMMAND)
            .map(|args| command(ctx, plan, checked, args)),
        "undo_camera_change" => parse::<UndoArgs>(checked, &UNDO_CAMERA_CHANGE)
            .map(|args| undo(ctx, plan, checked, args)),
        _ => None,
    };
}
fn parse<T: serde::de::DeserializeOwned>(checked: &mut Checked, spec: &ToolSpec) -> Option<T> {
    match parse_args(spec, &checked.call.args) {
        Ok(args) => Some(args),
        Err(issues) => {
            checked.issues.extend(issues);
            None
        }
    }
}
fn resolve(
    ctx: &Context,
    plan: &PlanState,
    checked: &mut Checked,
    text: &str,
) -> Option<CameraRef> {
    match plan.resolve_camera(ctx, "camera", text) {
        Ok(camera) => {
            checked.call.args["camera"] = json!(camera.selector());
            Some(camera)
        }
        Err(issue) => {
            checked.issue(issue);
            None
        }
    }
}
fn connected<'a>(
    ctx: &'a Context,
    plan: &PlanState,
    checked: &mut Checked,
    text: &str,
) -> Option<(CameraRef, Option<&'a CameraSnapshot>)> {
    let camera = resolve(ctx, plan, checked, text)?;
    if camera.connected().is_none() && !plan.connected.contains(camera.selector()) {
        checked.issue(Issue::error(
            Some("camera"),
            "Connect the camera first; propose connect_camera.",
            IssueCode::Unavailable,
        ));
        return None;
    }
    let snapshot = ctx
        .snapshot
        .cameras
        .iter()
        .find(|c| c.info.id == camera.selector());
    Some((camera, snapshot))
}
fn push_command(checked: &mut Checked, camera: CameraRef, command: SessionCommand) {
    if let Err(error) = command.validate() {
        checked.error("camera", sentence(&format!("{error:#}")));
        return;
    }
    checked.ops.push(Op::Command { camera, command });
}
fn connect(ctx: &Context, plan: &mut PlanState, checked: &mut Checked, args: ConnectArgs) {
    let Some(camera) = resolve(ctx, plan, checked, &args.camera) else {
        return;
    };
    let mut selector = camera.selector().to_owned();
    if plan.origin == Origin::Assistant
        && (has_userinfo(&selector) || crate::media::redact_url(&selector) != selector)
    {
        checked.error("camera", "Remove the password from the URL. Connect password-protected streams outside the assistant.");
    }
    let webcam = crate::media::is_host_camera(&selector);
    if !webcam && (args.size.is_some() || args.fps.is_some()) {
        checked.error(
            "size",
            "Only host webcams take size and fps when connecting; use set_camera_features.",
        );
    }
    if let Some(size) = args.size.as_deref() {
        let dimensions = size
            .trim()
            .split_once('x')
            .and_then(|(w, h)| Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?)));
        if !dimensions.is_some_and(|(w, h)| (16..=8192).contains(&w) && (16..=8192).contains(&h)) {
            checked.error(
                "size",
                "Enter a webcam size as WxH, with each side from 16 to 8,192 pixels.",
            );
        }
    }
    if args
        .fps
        .is_some_and(|fps| !fps.is_finite() || !(0.1..=240.0).contains(&fps))
    {
        checked.error("fps", "Choose a webcam frame rate from 0.1 to 240 fps.");
    }
    if !checked.ready() {
        return;
    }
    if camera.connected().is_some() {
        if args.size.is_some() || args.fps.is_some() {
            checked.error("camera", "This camera is already connected; use set_camera_features to change its size or frame rate.");
        }
        checked.summary = format!("{} is already connected.", camera.selector());
        return;
    }
    if let Some(size) = args.size {
        selector.push_str(if selector.contains('?') {
            "&size="
        } else {
            "?size="
        });
        selector.push_str(size.trim());
    }
    if let Some(fps) = args.fps {
        selector.push_str(if selector.contains('?') {
            "&fps="
        } else {
            "?fps="
        });
        selector.push_str(&fps.to_string());
    }
    let cmd = SessionCommand::Connect {
        camera: selector.clone(),
        timeout_ms: 3_000,
    };
    if let Err(error) = cmd.validate() {
        checked.error("camera", sentence(&format!("{error:#}")));
        return;
    }
    // Preserve the original selector in the call; Executor aliases it to
    // the ID returned by this exact connection.
    plan.connected.insert(camera.selector().to_owned());
    plan.connected.insert(selector.clone());
    checked.ops.push(Op::Connect {
        selector: selector.clone(),
    });
    checked.summary = format!("Connect {}.", super::redact::for_output_text(&selector));
}
fn has_userinfo(text: &str) -> bool {
    text.split_once("://").is_some_and(|(_, tail)| {
        tail.split(['/', '?', '#'])
            .next()
            .is_some_and(|authority| authority.contains('@'))
    })
}
fn live(ctx: &Context, plan: &mut PlanState, checked: &mut Checked, text: &str, start: bool) {
    let Some((camera, snapshot)) = connected(ctx, plan, checked, text) else {
        return;
    };
    if !start && (plan.is_forwarding(camera.selector()) || plan.active_jobs(camera.selector()) > 0)
    {
        let jobs = plan.active_jobs(camera.selector());
        let recording = if plan.is_forwarding(camera.selector()) {
            "end the recording"
        } else {
            "stop acquisition"
        };
        checked.error(
            "camera",
            format!(
                "Stopping would {recording}{}; stop them first.",
                if jobs > 0 {
                    format!(
                        " and cancel {} scheduled capture{}",
                        super::grouped(jobs as u64),
                        if jobs == 1 { "" } else { "s" }
                    )
                } else {
                    String::new()
                }
            ),
        );
        checked.risk = checked.risk.max(Risk::Disruptive);
        return;
    }
    if start && snapshot.is_some_and(|s| s.info.transport == Transport::Scanner) {
        checked.risk = checked.risk.max(Risk::Disruptive);
        checked.warning(
            "camera",
            "Starting a document scanner can feed and scan pages.",
        );
    }
    checked.summary = format!(
        "{} live view on {}.",
        if start { "Start" } else { "Stop" },
        camera.selector()
    );
    let selector = camera.selector().to_owned();
    push_command(
        checked,
        camera,
        if start {
            SessionCommand::Start
        } else {
            SessionCommand::Stop
        },
    );
    if checked.ready() {
        plan.streaming.insert(selector, start);
    }
}
fn features(ctx: &Context, plan: &mut PlanState, checked: &mut Checked, args: FeaturesArgs) {
    if !(1..=16).contains(&args.values.len()) {
        checked.error("values", "Choose from 1 to 16 feature values.");
    }
    let Some((camera, snapshot)) = connected(ctx, plan, checked, &args.camera) else {
        return;
    };
    let Some(snapshot) = snapshot else {
        checked.error(
            "camera",
            "Read this camera's features after connecting it, then add the settings step.",
        );
        return;
    };
    let mut values = Vec::new();
    let mut locked = false;
    for (index, setting) in args.values.iter().enumerate() {
        let field = format!("values.{index}.feature");
        let feature = snapshot
            .features
            .iter()
            .find(|f| f.name.eq_ignore_ascii_case(setting.feature.trim()));
        if risk::managed_name(&setting.feature)
            || feature.is_some_and(|f| risk::managed(&f.name, None, Some(&f.kind)))
        {
            checked.error(
                &field,
                format!(
                    "Capturefab manages {}; change it in the camera inspector.",
                    feature.map_or(setting.feature.as_str(), |f| f.name.as_str())
                ),
            );
            continue;
        }
        let Some(feature) = feature else {
            checked.error(
                &field,
                format!(
                    "Choose a feature this camera has; {} is not listed.",
                    setting.feature
                ),
            );
            continue;
        };
        let is_locked =
            plan.is_streaming(camera.selector()) && crate::genicam::stream_locked(&feature.name);
        locked |= is_locked;
        let unlocks_after_stop = snapshot.streaming
            && !plan.is_streaming(camera.selector())
            && crate::genicam::stream_locked(&feature.name);
        let permanently_read_only =
            snapshot.info.transport == Transport::Media && feature.name == "PixelFormat";
        if !feature.writable
            && (permanently_read_only
                || !((is_locked && args.restart_stream) || unlocks_after_stop))
        {
            checked.error(
                &field,
                format!(
                    "{} cannot be written in the camera's current state.",
                    feature.name
                ),
            );
        }
        if let Err(error) = feature.validate_input(&setting.value) {
            checked.error(
                &format!("values.{index}.value"),
                sentence(&format!("{error:#}")),
            );
        }
        if let Some(expect) = &setting.expect
            && !matches_value(expect, feature.value.as_ref())
            && !last_writer_is_us(ctx, plan, camera.selector(), &feature.name)
        {
            checked.error(&format!("values.{index}.expect"), format!("{} changed since it was observed; read its current value and review the change.", feature.name));
        }
        if snapshot
            .auto
            .as_ref()
            .is_some_and(|auto| auto.managed.contains(&feature.name))
        {
            checked.warning(
                &format!("values.{index}.value"),
                format!("Setting {} turns auto exposure off.", feature.name),
            );
        }
        if !risk::image_quality(&feature.name) {
            checked.risk = checked.risk.max(Risk::Disruptive);
            checked.reasons.push(format!(
                "{} is outside the image-quality allowlist.",
                feature.name
            ));
        }
        checked.call.args["values"][index]["feature"] = json!(feature.name);
        checked.facts.push((
            feature.name.clone(),
            format!(
                "{} → {}{}",
                feature.value.as_ref().map_or_else(
                    || "unavailable".into(),
                    |value| value
                        .as_str()
                        .map_or_else(|| value.to_string(), str::to_owned)
                ),
                setting.value,
                feature
                    .unit
                    .as_deref()
                    .map_or(String::new(), |unit| format!(" {unit}")),
            ),
        ));
        values.push(FeatureValue {
            feature: feature.name.clone(),
            value: setting.value.clone(),
        });
    }
    if locked && !args.restart_stream {
        checked.error("restart_stream", "These settings are locked while streaming; allow restart_stream or stop live view first.");
    }
    if !checked.ready() {
        return;
    }
    let changed: Vec<String> = values.iter().map(|v| v.feature.clone()).collect();
    batch(plan, checked, camera.clone(), values, locked);
    if checked.ready() {
        plan.changed_features
            .entry(camera.selector().into())
            .or_default()
            .extend(changed);
    }
}
fn matches_value(expect: &str, value: Option<&Value>) -> bool {
    let Some(value) = value else {
        return false;
    };
    match value {
        Value::String(live) => live == expect,
        Value::Number(live) if live.is_i64() => expect.parse::<i64>().ok() == live.as_i64(),
        Value::Number(live) if live.is_u64() => expect.parse::<u64>().ok() == live.as_u64(),
        Value::Number(live) => expect
            .parse::<f64>()
            .ok()
            .is_some_and(|expected| live.as_f64() == Some(expected)),
        Value::Bool(live) => expect.parse::<bool>().ok() == Some(*live),
        _ => false,
    }
}
fn last_writer_is_us(ctx: &Context, plan: &PlanState, camera: &str, feature: &str) -> bool {
    let Some(actor) = &plan.actor_id else {
        return false;
    };
    ctx.snapshot
        .activity
        .iter()
        .filter(|a| {
            a.camera.as_deref() == Some(camera)
                && a.state == ActivityState::Succeeded
                && matches!(
                    a.operation.as_str(),
                    "set"
                        | "set_many"
                        | "auto"
                        | "manual"
                        | "undo"
                        | "redo"
                        | "replay_camera_change"
                )
                && a.feature.as_deref().is_none_or(|name| name == feature)
        })
        .max_by_key(|a| a.id)
        .is_some_and(|a| &a.actor.id == actor)
}
fn batch(
    plan: &PlanState,
    checked: &mut Checked,
    camera: CameraRef,
    values: Vec<FeatureValue>,
    restart: bool,
) {
    let command = SessionCommand::SetMany { values };
    if let Err(error) = command.validate() {
        checked.error("values", sentence(&format!("{error:#}")));
        return;
    }
    if restart && (plan.is_forwarding(camera.selector()) || plan.active_jobs(camera.selector()) > 0)
    {
        checked.risk = checked.risk.max(Risk::Disruptive);
        checked.error("camera", "Stop the recording or stream and cancel scheduled captures before restarting live view.");
        return;
    }
    checked.summary = format!(
        "Set camera settings on {}{}.",
        camera.selector(),
        if restart {
            " and restart live view"
        } else {
            ""
        }
    );
    if restart {
        checked.ops.push(Op::Restartable {
            camera,
            commands: vec![SessionCommand::Stop, command, SessionCommand::Start],
        });
    } else {
        checked.ops.push(Op::Command { camera, command });
    }
}
fn roi(ctx: &Context, plan: &mut PlanState, checked: &mut Checked, args: RoiArgs) {
    let Some((camera, snapshot)) = connected(ctx, plan, checked, &args.camera) else {
        return;
    };
    let Some(snapshot) = snapshot else {
        checked.error(
            "camera",
            "Read this camera's features after connecting it, then add the region step.",
        );
        return;
    };
    let feature = |name: &str| snapshot.features.iter().find(|f| f.name == name);
    let (Some(width), Some(height), Some(x), Some(y)) = (
        feature("Width"),
        feature("Height"),
        feature("OffsetX"),
        feature("OffsetY"),
    ) else {
        checked.issue(Issue::error(Some("camera"), "This camera has no sensor ROI controls; use set_camera_features for its available size settings.", IssueCode::Unsupported));
        return;
    };
    let sensor = |maximum: &str, dimension: &FeatureInfo| {
        feature(maximum)
            .and_then(|f| f.value.as_ref())
            .and_then(Value::as_f64)
            .or(dimension.max)
    };
    let (Some(sensor_w), Some(sensor_h)) = (sensor("WidthMax", width), sensor("HeightMax", height))
    else {
        checked.error(
            "camera",
            "Read the camera's maximum sensor dimensions before changing its region.",
        );
        return;
    };
    if !sensor_w.is_finite() || !sensor_h.is_finite() || sensor_w < 1.0 || sensor_h < 1.0 {
        checked.error(
            "camera",
            "Read valid maximum sensor dimensions before changing its region.",
        );
        return;
    }
    let current = |f: &FeatureInfo| {
        f.value
            .as_ref()
            .and_then(Value::as_f64)
            .unwrap_or(f.min.unwrap_or(0.0))
    };
    let w = grid(
        width,
        if args.full {
            sensor_w
        } else {
            args.width.map_or(current(width), |v| v as f64)
        },
        sensor_w,
    );
    let h = grid(
        height,
        if args.full {
            sensor_h
        } else {
            args.height.map_or(current(height), |v| v as f64)
        },
        sensor_h,
    );
    let left = grid(
        x,
        if args.full {
            0.0
        } else {
            args.x.map_or(current(x), |v| v as f64)
        },
        (sensor_w - w).max(0.0),
    );
    let top = grid(
        y,
        if args.full {
            0.0
        } else {
            args.y.map_or(current(y), |v| v as f64)
        },
        (sensor_h - h).max(0.0),
    );
    for (key, value) in [("x", left), ("y", top), ("width", w), ("height", h)] {
        checked.call.args[key] = json!(value as i64);
    }
    let values = [
        ("OffsetX", 0.0),
        ("OffsetY", 0.0),
        ("Width", w),
        ("Height", h),
        ("OffsetX", left),
        ("OffsetY", top),
    ]
    .into_iter()
    .map(|(feature, value)| FeatureValue {
        feature: feature.into(),
        value: (value as i64).to_string(),
    })
    .collect();
    let streaming = plan.is_streaming(camera.selector());
    for f in [width, height, x, y] {
        if !f.writable && !snapshot.streaming && !streaming {
            checked.error(
                "camera",
                format!(
                    "{} cannot be written in the camera's current state.",
                    f.name
                ),
            );
        }
    }
    if !checked.ready() {
        return;
    }
    batch(plan, checked, camera.clone(), values, streaming);
    if checked.ready() {
        plan.changed_features
            .entry(camera.selector().into())
            .or_default()
            .extend(["Width", "Height", "OffsetX", "OffsetY"].map(str::to_owned));
        checked.facts.push((
            "Region".into(),
            format!(
                "{} x {} at {}, {} px",
                w as i64, h as i64, left as i64, top as i64
            ),
        ));
    }
}
fn grid(feature: &FeatureInfo, requested: f64, limit: f64) -> f64 {
    let min = feature.min.unwrap_or(0.0).min(limit);
    // The batch resets offsets first, so limits follow the new size and
    // position rather than the current selector-dependent access bounds.
    let max = limit.max(min);
    let inc = feature
        .inc
        .filter(|inc| inc.is_finite() && *inc > 0.0)
        .unwrap_or(1.0);
    let rounded = min + ((requested - min) / inc).round() * inc;
    // Clamp to a grid point, including when the maximum is not aligned.
    rounded
        .max(min)
        .min(min + ((max - min) / inc).floor() * inc)
}
fn auto(ctx: &Context, plan: &mut PlanState, checked: &mut Checked, args: AutoArgs) {
    let Some((camera, snapshot)) = connected(ctx, plan, checked, &args.camera) else {
        return;
    };
    if snapshot.is_some_and(|s| s.info.transport == Transport::Scanner) {
        checked.issue(Issue::error(
            Some("camera"),
            "Choose a camera with auto exposure; configure document-scanner settings with set_camera_features.",
            IssueCode::Unsupported,
        ));
    }
    if args
        .balance
        .is_some_and(|v| !v.is_finite() || !(0.0..=1.0).contains(&v))
    {
        checked.error("balance", "Choose an auto-exposure balance from 0 to 1.");
    }
    if args.mode != "auto" && args.balance.is_some() {
        checked.error(
            "balance",
            "Set balance only for auto mode; use null for hold or revert.",
        );
    }
    let command = match args.mode.as_str() {
        "auto" => SessionCommand::Auto {
            balance: args.balance,
        },
        "hold" => SessionCommand::Manual { revert: false },
        "revert" => SessionCommand::Manual { revert: true },
        _ => {
            checked.error("mode", "Choose auto, hold or revert.");
            return;
        }
    };
    if !checked.ready() {
        return;
    }
    checked.summary = format!(
        "{} auto exposure on {}.",
        match args.mode.as_str() {
            "auto" => "Enable",
            "hold" => "Hold",
            _ => "Revert",
        },
        camera.selector()
    );
    if args.mode == "auto" {
        checked.facts.push((
            "Next".into(),
            "Use wait_for auto_settled before inspecting the result.".into(),
        ));
    }
    push_command(checked, camera, command);
}
fn command(ctx: &Context, plan: &mut PlanState, checked: &mut Checked, args: CommandArgs) {
    if args.command == "AcquisitionStart" || args.command == "AcquisitionStop" {
        live(
            ctx,
            plan,
            checked,
            &args.camera,
            args.command == "AcquisitionStart",
        );
        return;
    }
    let Some((camera, snapshot)) = connected(ctx, plan, checked, &args.camera) else {
        return;
    };
    if !risk::command_allowed(&args.command) {
        checked.error(
            "command",
            format!(
                "Capturefab manages {}; change it in the camera inspector.",
                args.command
            ),
        );
        return;
    }
    let Some(feature) = snapshot.and_then(|s| {
        s.features
            .iter()
            .find(|f| f.name == args.command && f.kind == "Command")
    }) else {
        checked.error(
            "command",
            "Choose an existing command from this camera's controls.",
        );
        return;
    };
    checked.warning("command", "Camera commands cannot be undone.");
    checked.summary = format!("Run {} on {}.", feature.name, camera.selector());
    push_command(
        checked,
        camera,
        SessionCommand::Execute {
            feature: feature.name.clone(),
        },
    );
}
fn undo(ctx: &Context, plan: &mut PlanState, checked: &mut Checked, args: UndoArgs) {
    let Some((camera, _)) = connected(ctx, plan, checked, &args.camera) else {
        return;
    };
    if let Some(token) = &args.token
        && (token.is_empty() || token.len() > 256 || token.chars().any(char::is_control))
    {
        checked.error(
            "token",
            "Provide a valid camera_change token, or use null for this caller's latest token.",
        );
    }
    if !checked.ready() {
        return;
    }
    checked.summary = format!("Undo this caller's camera change on {}.", camera.selector());
    checked.ops.push(Op::Undo {
        camera,
        token: args.token,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{
        StepId, ToolCall,
        tests::{feature, fixture_context, fixture_snapshot},
    };
    fn checked(tool: &str, args: Value) -> Checked {
        let ctx = fixture_context(fixture_snapshot());
        let mut plan = PlanState::new(&ctx);
        let mut checked = Checked::new(
            StepId(1),
            &ToolCall {
                tool: tool.into(),
                args,
            },
        );
        check(&ctx, &mut plan, &mut checked, &|_, _| false);
        checked
    }
    #[test]
    fn batches_preflight_all_values_and_keep_one_atomic_op() {
        let result = checked(
            "set_camera_features",
            json!({"camera":"sim:0","values":[{"feature":"ExposureTime","value":"5000","expect":"10000"},{"feature":"Gain","value":"2","expect":null}],"restart_stream":false}),
        );
        assert!(result.ready(), "{:?}", result.issues);
        assert!(
            matches!(&result.ops[..],[Op::Command {command:SessionCommand::SetMany {values},..}] if values.len()==2)
        );
        assert!(
            result
                .issues
                .iter()
                .any(|i| i.message.contains("turns auto exposure off"))
        );
        let bad = checked(
            "set_camera_features",
            json!({"camera":"sim:0","values":[{"feature":"ExposureTime","value":"5000","expect":null},{"feature":"GevSCDA","value":"1","expect":null}],"restart_stream":false}),
        );
        assert!(!bad.ready() && bad.ops.is_empty());
    }
    #[test]
    fn locked_features_need_restart_and_cannot_interrupt_jobs() {
        let args = json!({"camera":"sim:0","values":[{"feature":"Width","value":"640","expect":null}],"restart_stream":false});
        let bad = checked("set_camera_features", args.clone());
        assert!(
            bad.issues
                .iter()
                .any(|i| i.field.as_deref() == Some("restart_stream"))
        );
        let mut args = args;
        args["restart_stream"] = json!(true);
        let busy = checked("set_camera_features", args);
        assert!(!busy.ready() && busy.ops.is_empty());
    }
    #[test]
    fn planned_stop_unlocks_settings_and_a_safe_restart_preserves_live_view() {
        let mut snapshot = fixture_snapshot();
        snapshot.cameras[0].jobs.clear();
        let ctx = fixture_context(snapshot);
        let mut plan = PlanState::new(&ctx);
        let call = ToolCall {
            tool: "set_camera_features".into(),
            args: json!({"camera":"sim:0","values":[{"feature":"Width","value":"640","expect":null}],"restart_stream":true}),
        };
        let mut restart = Checked::new(StepId(1), &call);
        check(&ctx, &mut plan, &mut restart, &|_, _| false);
        assert!(restart.ready(), "{:?}", restart.issues);
        assert!(
            matches!(&restart.ops[..], [Op::Restartable { commands, .. }] if matches!(&commands[..], [SessionCommand::Stop, SessionCommand::SetMany {..}, SessionCommand::Start]))
        );
        let mut stop = Checked::new(
            StepId(2),
            &ToolCall {
                tool: "stop_live_view".into(),
                args: json!({"camera":"sim:0"}),
            },
        );
        check(&ctx, &mut plan, &mut stop, &|_, _| false);
        assert!(stop.ready());
        assert!(!plan.is_streaming("sim:0"));
        let mut call = call;
        call.args["restart_stream"] = json!(false);
        let mut setting = Checked::new(StepId(3), &call);
        check(&ctx, &mut plan, &mut setting, &|_, _| false);
        assert!(setting.ready(), "{:?}", setting.issues);
        assert!(matches!(
            &setting.ops[..],
            [Op::Command {
                command: SessionCommand::SetMany { .. },
                ..
            }]
        ));
    }
    #[test]
    fn stale_expectations_refuse_before_any_writes() {
        let result = checked(
            "set_camera_features",
            json!({"camera":"sim:0","values":[{"feature":"Gain","value":"2","expect":"9"}],"restart_stream":false}),
        );
        assert!(
            result
                .issues
                .iter()
                .any(|i| i.field.as_deref() == Some("values.0.expect"))
        );
        assert!(result.ops.is_empty());
    }
    #[test]
    fn expected_integer_values_do_not_lose_precision() {
        assert!(matches_value(
            "9007199254740993",
            Some(&json!(9_007_199_254_740_993u64))
        ));
        assert!(!matches_value(
            "9007199254740992",
            Some(&json!(9_007_199_254_740_993u64))
        ));
        assert!(matches_value("10000", Some(&json!(10000.0))));
    }

    #[test]
    fn failed_activity_does_not_waive_an_expected_value_conflict() {
        let mut ctx = fixture_context(fixture_snapshot());
        let entry = serde_json::from_value(json!({
            "id": 1, "origin": "client", "actor": {"id":"our-run","name":"Assistant","kind":"agent"},
            "operation":"set", "camera":"sim:0", "feature":"Gain", "label":"Set Gain = 2",
            "state":"failed", "submitted_at_ms":ctx.now_ms, "started_at_ms":ctx.now_ms,
            "finished_at_ms":ctx.now_ms, "detail":null, "file_count":0, "files":[], "job":null,
        })).unwrap();
        ctx.snapshot.activity.push(entry);
        let mut plan = PlanState::new(&ctx);
        plan.actor_id = Some("our-run".into());
        assert!(!last_writer_is_us(&ctx, &plan, "sim:0", "Gain"));
        ctx.snapshot.activity[0].state = ActivityState::Succeeded;
        assert!(last_writer_is_us(&ctx, &plan, "sim:0", "Gain"));
    }

    #[test]
    fn roi_grid_uses_the_bounds_after_resetting_offsets() {
        let mut width = feature("Width", "Integer", json!(640));
        width.min = Some(64.0);
        width.max = Some(1280.0);
        width.inc = Some(2.0);
        assert_eq!(grid(&width, 1920.0, 1920.0), 1920.0);
        let mut offset = feature("OffsetX", "Integer", json!(0));
        offset.min = Some(0.0);
        offset.max = Some(0.0);
        offset.inc = Some(4.0);
        assert_eq!(grid(&offset, 10.0, 1280.0), 12.0);
    }
    #[test]
    fn commands_use_the_allowlist_and_acquisition_checks() {
        for name in ["DeviceReset", "UserSetSave", "Unknown"] {
            let result = checked(
                "run_camera_command",
                json!({"camera":"sim:0","command":name}),
            );
            assert!(!result.ready() && result.ops.is_empty());
        }
        let result = checked(
            "run_camera_command",
            json!({"camera":"sim:0","command":"TriggerSoftware"}),
        );
        assert!(result.ready(), "{:?}", result.issues);
        assert!(matches!(
            &result.ops[..],
            [Op::Command {
                command: SessionCommand::Execute { .. },
                ..
            }]
        ));
        let result = checked(
            "run_camera_command",
            json!({"camera":"sim:0","command":"AcquisitionStop"}),
        );
        assert!(!result.ready());
    }
    #[test]
    fn auto_modes_and_undo_are_pinned() {
        for (mode, revert) in [("hold", false), ("revert", true)] {
            let result = checked(
                "set_auto_exposure",
                json!({"camera":"sim:0","mode":mode,"balance":null}),
            );
            assert!(
                matches!(&result.ops[..],[Op::Command {camera:CameraRef::Connected(id),command:SessionCommand::Manual {revert:r}}] if id=="sim:0" && *r==revert)
            );
        }
        assert!(
            !checked(
                "set_auto_exposure",
                json!({"camera":"sim:0","mode":"hold","balance":0.5})
            )
            .ready()
        );
        let result = checked("undo_camera_change", json!({"camera":"sim:0","token":null}));
        assert!(
            matches!(&result.ops[..],[Op::Undo {camera:CameraRef::Connected(id),token:None}] if id=="sim:0")
        );
        let result = checked(
            "set_auto_exposure",
            json!({"camera":"sim:0","mode":"auto","balance":0.25}),
        );
        assert!(result.ready());
        assert!(
            matches!(&result.ops[..], [Op::Command { command:SessionCommand::Auto {balance:Some(v)}, .. }] if *v == 0.25)
        );
    }
    #[test]
    fn roi_rounds_on_the_grid_and_orders_offset_resets_before_size() {
        let mut snapshot = fixture_snapshot();
        let camera = &mut snapshot.cameras[0];
        camera.streaming = false;
        camera.jobs.clear();
        let mut features = camera.features.to_vec();
        for (name, value, max, inc) in [
            ("OffsetX", 0, 1280.0, 2.0),
            ("OffsetY", 0, 600.0, 2.0),
            ("WidthMax", 1920, 1920.0, 1.0),
            ("HeightMax", 1080, 1080.0, 1.0),
        ] {
            let mut f = feature(name, "Integer", json!(value));
            f.min = Some(0.0);
            f.max = Some(max);
            f.inc = Some(inc);
            features.push(f);
        }
        camera.features = features.into();
        let ctx = fixture_context(snapshot);
        let mut plan = PlanState::new(&ctx);
        let mut result = Checked::new(
            StepId(1),
            &ToolCall {
                tool: "set_camera_roi".into(),
                args: json!({"camera":"sim:0","full":false,"x":5,"y":7,"width":641,"height":480}),
            },
        );
        check(&ctx, &mut plan, &mut result, &|_, _| false);
        assert!(result.ready(), "{:?}", result.issues);
        let Op::Command {
            command: SessionCommand::SetMany { values },
            ..
        } = &result.ops[0]
        else {
            panic!("expected batch")
        };
        assert_eq!(
            values
                .iter()
                .map(|v| v.feature.as_str())
                .collect::<Vec<_>>(),
            [
                "OffsetX", "OffsetY", "Width", "Height", "OffsetX", "OffsetY"
            ]
        );
        assert_eq!(values[2].value, "642");
        assert_eq!(values[4].value, "6");
    }
    #[test]
    fn connect_webcam_options_and_other_camera_refusals() {
        let result = checked(
            "connect_camera",
            json!({"camera":"gige:192.168.1.20","size":"1920x1080","fps":30}),
        );
        assert!(!result.ready());
        assert!(result.issues.iter().any(|i| i.message
            == "Only host webcams take size and fps when connecting; use set_camera_features."));
        let result = checked(
            "connect_camera",
            json!({"camera":"sim:8","size":null,"fps":null}),
        );
        assert!(result.ready(), "{:?}", result.issues);
        assert!(matches!(&result.ops[..],[Op::Connect {selector}] if selector=="sim:8"));
    }
}
