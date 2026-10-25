//! Bounded captures and forwarding: pure preflight builds the exact session
//! commands the executor sends. All outputs use the caller's storage policy.
use super::{
    CameraRef, FieldKind, FieldSpec, Issue, IssueCode, Risk, ToolKind, ToolSpec, camera_name,
    context::Context,
    exec::{Checked, Op, PlanState},
    format_bytes, format_duration_field, grouped, parse_args, schema as s, sentence, values,
};
use crate::{naming::OnExists, session::SessionCommand, types::Transport};
use serde::Deserialize;
use serde_json::{Value, json};

const FORMATS: &[(&str, &str)] = &[
    ("png", "PNG"),
    ("jpeg", "JPEG"),
    ("raw", "Raw"),
    ("pgm", "PGM"),
    ("ppm", "PPM"),
];
const CODECS: &[(&str, &str)] = &[("h264", "H.264"), ("hevc", "H.265"), ("av1", "AV1")];
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
const CAMERA: FieldSpec = field(
    "camera",
    "Camera",
    FieldKind::Camera,
    false,
    "A connected or discovered camera.",
);
const DURATION: FieldSpec = field(
    "duration",
    "Duration",
    FieldKind::Duration,
    true,
    "Use units, such as 90s, 15m or 1h30m; at least 1 second.",
);
const FILE: FieldSpec = field(
    "file_name",
    "File name",
    FieldKind::FileName,
    true,
    "A single file name or template, without folders. Existing files are numbered.",
);
const DESTINATION: FieldSpec = field(
    "destination",
    "Destination",
    FieldKind::Destination,
    true,
    "Empty saves in the capture folder.",
);
const TAGS: FieldSpec = field(
    "tags",
    "Tags",
    FieldKind::Tags,
    true,
    "Up to 16 short key/value tags.",
);
const CODEC: FieldSpec = field(
    "codec",
    "Codec",
    FieldKind::Choice(CODECS),
    true,
    "Uses the Video settings when empty.",
);
const FPS: FieldSpec = field(
    "fps",
    "Frames per second",
    FieldKind::Number {
        min: 0.1,
        max: 240.0,
    },
    true,
    "Output frame rate; faster output repeats frames.",
);
const BITRATE: FieldSpec = field(
    "bitrate",
    "Bitrate",
    FieldKind::Text,
    true,
    "For example 4M; uses the Video settings when empty.",
);
const FORMAT: FieldSpec = field(
    "format",
    "Format",
    FieldKind::Choice(FORMATS),
    true,
    "PNG by default.",
);

pub const CAPTURE_PHOTOS: ToolSpec = ToolSpec {
    name: "capture_photos",
    title: "Capture photos",
    kind: ToolKind::Capture,
    risk: Risk::Bounded,
    needs_session: true,
    needs_vision: false,
    description: "Save 1..1000 photos (default 1) as png (default), jpeg, raw, pgm or ppm. Connects a discovered camera first. File names are single templates; existing files are numbered. Scanners consume pages and wait up to 60 seconds per page.",
    schema: photos_schema,
    fields: &[
        CAMERA,
        field(
            "count",
            "Photos",
            FieldKind::Integer { min: 1, max: 1000 },
            true,
            "One photo by default.",
        ),
        FORMAT,
        FILE,
        DESTINATION,
        TAGS,
    ],
};
pub const START_RECORDING: ToolSpec = ToolSpec {
    name: "start_recording",
    title: "Start recording",
    kind: ToolKind::Capture,
    risk: Risk::Bounded,
    needs_session: true,
    needs_vision: false,
    description: "Record video with a worker-enforced duration, such as 90s, 15m or 1h30m (1 second..10 years). Codec, fps and bitrate default to Video settings. Without a duration stops before the 512 MiB reservation is reached. One recording or stream per camera. Keep Capturefab open and the computer awake. Existing files are numbered.",
    schema: recording_schema,
    fields: &[
        CAMERA,
        DURATION,
        FILE,
        DESTINATION,
        CODEC,
        FPS,
        BITRATE,
        TAGS,
    ],
};
pub const STOP_RECORDING: ToolSpec = ToolSpec {
    name: "stop_recording",
    title: "Stop recording or streaming",
    kind: ToolKind::Capture,
    risk: Risk::Bounded,
    needs_session: true,
    needs_vision: false,
    description: "Finish this camera's current recording or stream. Stopping work started by someone else needs approval. It does not cancel scheduled captures.",
    schema: camera_schema,
    fields: &[CAMERA],
};
pub const SCHEDULE_CAPTURES: ToolSpec = ToolSpec {
    name: "schedule_captures",
    title: "Schedule captures",
    kind: ToolKind::Capture,
    risk: Risk::Bounded,
    needs_session: true,
    needs_vision: false,
    description: "Save photos every interval (units required, such as 5m), ending by exactly one of duration, count or until. Times accept HH:MM locally or RFC3339 with an offset and are pinned on first check. At most 100000 photos and 32 active schedules per camera. Keep Capturefab open and awake; missed slots catch up on wake. Scanners consume pages.",
    schema: schedule_schema,
    fields: &[
        CAMERA,
        field(
            "every",
            "Every",
            FieldKind::Duration,
            false,
            "A positive interval, such as 5m.",
        ),
        DURATION,
        field(
            "count",
            "Photos",
            FieldKind::Integer {
                min: 1,
                max: 100000,
            },
            true,
            "Choose one way to end.",
        ),
        field(
            "until",
            "Until",
            FieldKind::Time,
            true,
            "HH:MM locally or RFC3339 with offset.",
        ),
        field(
            "start_at",
            "Start at",
            FieldKind::Time,
            true,
            "Empty starts now; a local time is pinned on first check.",
        ),
        FORMAT,
        FILE,
        DESTINATION,
        TAGS,
    ],
};
pub const CANCEL_JOB: ToolSpec = ToolSpec {
    name: "cancel_job",
    title: "Cancel scheduled captures",
    kind: ToolKind::Capture,
    risk: Risk::Bounded,
    needs_session: true,
    needs_vision: false,
    description: "Cancel one scheduled capture job by the ID returned by list_jobs. Cancelling a job started by someone else needs approval. Files already saved stay saved.",
    schema: cancel_schema,
    fields: &[
        CAMERA,
        field(
            "job_id",
            "Job",
            FieldKind::Integer {
                min: 1,
                max: i64::MAX,
            },
            false,
            "The schedule's ID from list_jobs.",
        ),
    ],
};
pub const STREAM_TO_URL: ToolSpec = ToolSpec {
    name: "stream_to_url",
    title: "Stream to a URL",
    kind: ToolKind::Capture,
    risk: Risk::Disruptive,
    needs_session: true,
    needs_vision: false,
    description: "Send video to an RTSP, RTMP or SRT URL. Always needs approval. Never put a password or stream key in the URL; enter it in Stream password instead. Duration uses units; empty runs until stopped. One recording or stream per camera. Codec, fps and bitrate use Video settings by default.",
    schema: stream_schema,
    fields: &[
        CAMERA,
        field(
            "url",
            "Stream URL",
            FieldKind::Url,
            false,
            "A destination URL without credentials.",
        ),
        DURATION,
        CODEC,
        FPS,
        BITRATE,
        FieldSpec {
            key: "password",
            label: "Stream password",
            kind: FieldKind::Secret,
            optional: true,
            help: "Used only when starting the stream; never sent to the assistant.",
            ui_only: true,
        },
    ],
};
fn camera_schema() -> Value {
    s::object(vec![(
        "camera",
        s::string("Camera ID or exact friendly name."),
    )])
}
fn tags_schema() -> Value {
    s::nullable(s::array(
        s::object(vec![
            ("key", s::string("Lowercase tag key, at most 32 bytes.")),
            ("value", s::string("Tag value, at most 256 bytes.")),
        ]),
        "At most 16 tags; null for none.",
    ))
}
fn photo_properties() -> Vec<(&'static str, Value)> {
    vec![
        (
            "format",
            s::nullable(s::choice(
                &["png", "jpeg", "raw", "pgm", "ppm"],
                "PNG by default.",
            )),
        ),
        (
            "file_name",
            s::nullable(s::string("Single file name or template; no folders.")),
        ),
        (
            "destination",
            s::nullable(s::string(
                "Saved destination name; null for the capture folder.",
            )),
        ),
        ("tags", tags_schema()),
    ]
}
fn forward_properties() -> Vec<(&'static str, Value)> {
    vec![
        (
            "duration",
            s::nullable(s::string("Duration with units, at least 1 second.")),
        ),
        (
            "codec",
            s::nullable(s::choice(
                &["h264", "h265", "hevc", "av1"],
                "Video settings by default; h265 normalizes to hevc.",
            )),
        ),
        (
            "fps",
            s::nullable(s::number("0.1..240; Video settings by default.")),
        ),
        (
            "bitrate",
            s::nullable(s::string(
                "Video bitrate such as 4M; Video settings by default.",
            )),
        ),
    ]
}
fn photos_schema() -> Value {
    let mut p = vec![
        ("camera", s::string("Camera ID or exact friendly name.")),
        (
            "count",
            s::nullable(s::integer("1..1000 photos; default 1.")),
        ),
    ];
    p.extend(photo_properties());
    s::object(p)
}
fn recording_schema() -> Value {
    let mut p = vec![("camera", s::string("Camera ID or exact friendly name."))];
    p.extend(forward_properties());
    p.extend(vec![
        (
            "file_name",
            s::nullable(s::string("Recording template; default MP4.")),
        ),
        (
            "destination",
            s::nullable(s::string(
                "Saved destination; null for local capture folder.",
            )),
        ),
        ("tags", tags_schema()),
    ]);
    s::object(p)
}
fn schedule_schema() -> Value {
    let mut p = vec![
        ("camera", s::string("Camera ID or exact friendly name.")),
        (
            "every",
            s::string("Positive interval with units, such as 5m."),
        ),
        (
            "duration",
            s::nullable(s::string("Duration with units, at least 1 second.")),
        ),
        ("count", s::nullable(s::integer("1..100000 photos."))),
        (
            "until",
            s::nullable(s::string("HH:MM locally or RFC3339 with offset.")),
        ),
        (
            "start_at",
            s::nullable(s::string(
                "HH:MM locally or RFC3339 with offset; null for now.",
            )),
        ),
    ];
    p.extend(photo_properties());
    s::object(p)
}
fn cancel_schema() -> Value {
    s::object(vec![
        ("camera", s::string("Connected camera.")),
        ("job_id", s::integer("Schedule ID from list_jobs.")),
    ])
}
fn stream_schema() -> Value {
    let mut p = vec![
        ("camera", s::string("Connected camera.")),
        (
            "url",
            s::string("RTSP, RTMP or SRT destination without credentials."),
        ),
    ];
    p.extend(forward_properties());
    s::object(p)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Tag {
    key: String,
    value: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Photos {
    camera: String,
    count: Option<u64>,
    format: Option<String>,
    file_name: Option<String>,
    destination: Option<String>,
    tags: Option<Vec<Tag>>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Recording {
    camera: String,
    duration: Option<String>,
    file_name: Option<String>,
    destination: Option<String>,
    codec: Option<String>,
    fps: Option<f64>,
    bitrate: Option<String>,
    tags: Option<Vec<Tag>>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Schedule {
    camera: String,
    every: String,
    duration: Option<String>,
    count: Option<u64>,
    until: Option<String>,
    start_at: Option<String>,
    format: Option<String>,
    file_name: Option<String>,
    destination: Option<String>,
    tags: Option<Vec<Tag>>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Camera {
    camera: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Cancel {
    camera: String,
    job_id: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Stream {
    camera: String,
    url: String,
    duration: Option<String>,
    codec: Option<String>,
    fps: Option<f64>,
    bitrate: Option<String>,
}

pub(crate) fn check(
    ctx: &Context,
    plan: &mut PlanState,
    checked: &mut Checked,
    _secrets_present: &dyn Fn(super::StepId, &str) -> bool,
) {
    let Some(spec) = super::spec(&checked.call.tool) else {
        return;
    };
    macro_rules! args {
        ($t:ty) => {
            match parse_args::<$t>(spec, &checked.call.args) {
                Ok(a) => a,
                Err(issues) => {
                    checked.issues.extend(issues);
                    return;
                }
            }
        };
    }
    match checked.call.tool.as_str() {
        "capture_photos" => {
            let a = args!(Photos);
            photos(ctx, plan, checked, a);
        }
        "start_recording" => {
            let a = args!(Recording);
            recording(ctx, plan, checked, a);
        }
        "schedule_captures" => {
            let a = args!(Schedule);
            schedule(ctx, plan, checked, a);
        }
        "stream_to_url" => {
            let a = args!(Stream);
            stream(ctx, plan, checked, a);
        }
        "stop_recording" => {
            let a = args!(Camera);
            if let Some(camera) = camera_for(ctx, plan, checked, &a.camera, false) {
                checked.summary = format!("Stop recording or streaming on {}.", camera.selector());
                let forwarding_id = plan.forwarding_ids.get(camera.selector()).cloned();
                if plan.forwarding.contains(camera.selector()) && forwarding_id.is_none() {
                    checked.error("camera", "This camera worker did not identify its recording. Stop it in the camera inspector.");
                }
                if plan.forwarding.contains(camera.selector()) {
                    checked.ops.push(Op::StopForward {
                        camera: camera.clone(),
                        forwarding_id: forwarding_id.flatten(),
                    });
                } else {
                    checked.summary =
                        format!("No recording or stream is active on {}.", camera.selector());
                }
                plan.forwarding.remove(camera.selector());
                plan.forwarding_ids.remove(camera.selector());
            }
        }
        "cancel_job" => {
            let a = args!(Cancel);
            if let Some(camera) = camera_for(ctx, plan, checked, &a.camera, false) {
                let live = ctx
                    .snapshot
                    .cameras
                    .iter()
                    .find(|c| c.info.id == camera.selector());
                let key = (camera.selector().to_owned(), a.job_id);
                if plan.canceled_jobs.contains(&key) {
                    checked.error(
                        "job_id",
                        "This schedule is already canceled by an earlier step.",
                    );
                } else if !live
                    .is_some_and(|c| c.jobs.iter().any(|j| j.id == a.job_id && j.active()))
                {
                    checked.issue(Issue::error(
                        Some("job_id"),
                        "Choose an active schedule from list_jobs.",
                        IssueCode::Unavailable,
                    ));
                }
                checked.summary = format!("Cancel schedule {} on {}.", a.job_id, camera.selector());
                checked.ops.push(Op::Command {
                    camera: camera.clone(),
                    command: SessionCommand::CancelJob { id: a.job_id },
                });
                if checked.ready() {
                    plan.canceled_jobs.insert(key);
                    if let Some(jobs) = plan.jobs.get_mut(camera.selector()) {
                        *jobs = jobs.saturating_sub(1);
                    }
                }
            }
        }
        _ => {}
    }
}

fn camera_for(
    ctx: &Context,
    plan: &mut PlanState,
    checked: &mut Checked,
    text: &str,
    connect: bool,
) -> Option<CameraRef> {
    match plan.resolve_camera(ctx, "camera", text) {
        Ok(camera) => {
            checked.call.args["camera"] = json!(camera.selector());
            if camera.connected().is_none() && !plan.connected.contains(camera.selector()) {
                if connect {
                    checked.ops.push(Op::Connect {
                        selector: camera.selector().into(),
                    });
                    plan.connected.insert(camera.selector().into());
                } else {
                    checked.issue(Issue::error(
                        Some("camera"),
                        "Connect the camera first; propose connect_camera.",
                        IssueCode::Unavailable,
                    ));
                }
            }
            Some(camera)
        }
        Err(issue) => {
            checked.issue(issue);
            None
        }
    }
}
fn scanner(ctx: &Context, camera: &CameraRef) -> bool {
    ctx.snapshot
        .cameras
        .iter()
        .map(|c| &c.info)
        .chain(ctx.snapshot.devices.iter())
        .find(|info| info.id == camera.selector())
        .is_some_and(|info| info.transport == Transport::Scanner)
}
fn tags(checked: &mut Checked, values: Option<Vec<Tag>>) -> crate::context::Context {
    let mut tags = crate::context::Context::new();
    for tag in values.unwrap_or_default() {
        if tags.insert(tag.key, tag.value).is_some() {
            checked.error("tags", "Use each tag key only once.");
        }
    }
    if let Err(error) = crate::context::validate(&tags) {
        checked.error("tags", sentence(&error.to_string()));
    }
    tags
}
fn count(checked: &mut Checked, value: u64, max: u64) -> u32 {
    if !(1..=max).contains(&value) {
        checked.error(
            "count",
            format!("Enter a whole number of photos from 1 to {}.", grouped(max)),
        );
    }
    value.clamp(1, max) as u32
}
fn duration(checked: &mut Checked, field: &str, text: &str, min: u64) -> Option<u64> {
    match values::duration(field, text, min) {
        Ok((ms, canonical)) => {
            checked.call.args[field] = json!(canonical);
            Some(ms)
        }
        Err(issue) => {
            checked.issue(issue);
            None
        }
    }
}
fn output(
    ctx: &Context,
    plan: &PlanState,
    checked: &mut Checked,
    name: String,
    destination: Option<String>,
) -> Option<(String, Option<String>, Option<u64>)> {
    checked.call.args["file_name"] = json!(name);
    if let Some(name) = destination {
        let Some(found) = plan
            .destinations
            .iter()
            .find(|d| d.name.eq_ignore_ascii_case(&name))
        else {
            checked.issue(Issue::error(
                Some("destination"),
                "Choose a saved destination from list_destinations.",
                IssueCode::Unavailable,
            ));
            return None;
        };
        checked.call.args["destination"] = json!(found.name);
        checked.destination = Some(found.clone());
        if !ctx.compiled.supports(&found.kind) {
            checked.issue(Issue::error(
                Some("destination"),
                format!(
                    "Choose a destination supported by this build; {} destinations need --features {}.",
                    found.kind, found.kind
                ),
                IssueCode::Unsupported,
            ));
        } else if let Some(problem) = &found.problem {
            checked.issue(Issue::error(
                Some("destination"),
                sentence(problem),
                IssueCode::Unavailable,
            ));
        }
        checked
            .facts
            .push(("Destination".into(), found.location.clone()));
        Some((
            checked.call.args["file_name"].as_str().unwrap().into(),
            Some(found.name.clone()),
            found.free_bytes,
        ))
    } else {
        match values::capture_path(&ctx.capture_root, &name) {
            Ok(path) => {
                checked.output_root = Some(ctx.capture_root.clone());
                checked
                    .facts
                    .push(("Folder".into(), super::redact::home_path(&ctx.capture_root)));
                Some((path.to_string_lossy().into(), None, ctx.storage.free_disk))
            }
            Err(issue) => {
                checked.issue(issue);
                None
            }
        }
    }
}
struct Budget<'a> {
    bytes: u64,
    files: u64,
    exact: bool,
    field: &'a str,
    free_disk: Option<u64>,
}

fn budget(ctx: &Context, plan: &mut PlanState, checked: &mut Checked, estimate: Budget<'_>) {
    let Budget {
        bytes,
        files,
        exact,
        field,
        free_disk,
    } = estimate;
    let policy = &ctx.storage.effective;
    let free = ctx.storage.free_budget().saturating_sub(plan.planned_bytes);
    let free_files = u64::from(policy.max_files)
        .saturating_sub(ctx.storage.files)
        .saturating_sub(plan.planned_files);
    let disk_full = free_disk.is_some_and(|free| {
        bytes
            .checked_add(plan.planned_bytes)
            .and_then(|needed| needed.checked_add(ctx.storage.reserved_bytes))
            .and_then(|needed| needed.checked_add(crate::storage::FREE_SPACE_FLOOR))
            .is_none_or(|needed| free < needed)
    });
    if exact && bytes > policy.max_bytes {
        let maximum = checked.call.args["bitrate"]
            .as_str()
            .and_then(|bitrate| crate::media::max_duration_for(bitrate, policy.max_bytes).ok())
            .filter(|duration| *duration > 0);
        let advice = maximum.map_or_else(
            || "Lower the bitrate or raise the budget in Settings.".into(),
            |duration| {
                format!(
                    "Record at most {}, lower the bitrate, or raise the budget in Settings.",
                    crate::scheduling::format_duration(duration)
                )
            },
        );
        checked.issue(Issue::error(
            Some(field),
            format!(
                "This recording needs up to {} but the storage budget is {}. {advice}",
                format_bytes(bytes),
                format_bytes(policy.max_bytes)
            ),
            IssueCode::StorageFull,
        ));
    }
    if bytes > free || files > free_files || disk_full {
        if policy.on_full == "delete-oldest" {
            checked.warning(
                field,
                "Makes room by permanently deleting the oldest captures.",
            );
        } else {
            if files > free_files {
                let subject = match checked.call.tool.as_str() {
                    "schedule_captures" => "This schedule saves",
                    "capture_photos" => "This capture saves",
                    _ => "This saves",
                };
                let unit = if exact { "files" } else { "photos" };
                checked.issue(Issue::error(
                    Some(field),
                    format!(
                        "{subject} {} {unit} but only {} more files fit in the storage budget.",
                        grouped(files),
                        grouped(free_files)
                    ),
                    IssueCode::StorageFull,
                ));
            }
            if bytes > free {
                let message = format!(
                    "Needs up to {}, but only {} of the {} budget is free.",
                    format_bytes(bytes),
                    format_bytes(free),
                    format_bytes(policy.max_bytes)
                );
                if exact {
                    checked.issue(Issue::error(Some(field), message, IssueCode::StorageFull));
                } else {
                    checked.warning(field, message);
                }
            }
        }
    }
    if disk_full && policy.on_full != "delete-oldest" {
        checked.issue(Issue::error(
            Some(field),
            format!(
                "Free space at the destination cannot cover the {} this may need and leave 512 MiB free after existing reservations and earlier planned captures.",
                format_bytes(bytes)
            ),
            IssueCode::StorageFull,
        ));
    }
    checked.facts.push((
        "Storage".into(),
        format!(
            "≈ {} · {} of {} free",
            format_bytes(bytes),
            format_bytes(free),
            format_bytes(policy.max_bytes)
        ),
    ));
    if checked.ready() {
        plan.planned_bytes = plan.planned_bytes.saturating_add(bytes);
        plan.planned_files = plan.planned_files.saturating_add(files);
    }
}
fn photo_estimate(
    ctx: &Context,
    plan: &PlanState,
    checked: &mut Checked,
    camera: &CameraRef,
    count: u32,
) -> u64 {
    let features = ctx
        .snapshot
        .cameras
        .iter()
        .find(|c| c.info.id == camera.selector())
        .map(|c| &c.features);
    let changed = plan
        .changed_features
        .get(camera.selector())
        .is_some_and(|features| !features.is_empty());
    if changed {
        checked.warning(
            "camera",
            "Earlier camera setting changes may increase photo sizes; the storage estimate uses maximum sensor dimensions.",
        );
    }
    let positive = |value: f64| (value.is_finite() && value > 0.0).then(|| value.ceil() as u64);
    let dim = |name: &str| {
        let feature = features.and_then(|features| features.iter().find(|f| f.name == name));
        let current = feature
            .and_then(|f| f.value.as_ref())
            .and_then(Value::as_f64)
            .and_then(positive)
            .unwrap_or(4096);
        if !changed {
            return current;
        }
        let sensor_max = features
            .and_then(|features| features.iter().find(|f| f.name == format!("{name}Max")))
            .and_then(|f| f.value.as_ref())
            .and_then(Value::as_f64)
            .and_then(positive);
        let feature_max = feature.and_then(|f| f.max).and_then(positive);
        sensor_max
            .into_iter()
            .chain(feature_max)
            .max()
            .map_or_else(|| current.max(4096), |maximum| current.max(maximum))
    };
    dim("Width")
        .saturating_mul(dim("Height"))
        .saturating_mul(4)
        .saturating_add(65536)
        .saturating_mul(u64::from(count))
}

fn photo_format(ctx: &Context, checked: &mut Checked, format: &str) {
    if format == "jpeg" && !ctx.compiled.jpeg {
        checked.issue(Issue::error(
            Some("format"),
            "Choose PNG, or use a build with JPEG support (--features jpeg).",
            IssueCode::Unsupported,
        ));
    }
}

fn file_fact(ctx: &Context, checked: &mut Checked, camera: &CameraRef, output: &str) {
    let label = ctx
        .snapshot
        .cameras
        .iter()
        .map(|camera| &camera.info)
        .chain(ctx.snapshot.devices.iter())
        .find(|info| info.id == camera.selector())
        .map(crate::naming::camera_label)
        .unwrap_or_else(|| "camera".into());
    // Tool file names refuse ':', so they never hold {tag:KEY}.
    let preview = crate::naming::preview(
        output,
        &label,
        &Default::default(),
        ctx.zone.at(ctx.now_ms).with_timezone(&chrono::Local),
    );
    let preview = match &checked.destination {
        Some(destination) => {
            format!("{}/{preview}", destination.location.trim_end_matches('/'))
        }
        None => preview,
    };
    checked.facts.push((
        "File".into(),
        super::redact::home_path(std::path::Path::new(&preview)),
    ));
}

fn photos(ctx: &Context, plan: &mut PlanState, checked: &mut Checked, a: Photos) {
    let n = count(checked, a.count.unwrap_or(1), 1000);
    let format = a.format.as_deref().unwrap_or("png");
    photo_format(ctx, checked, format);
    let name = match values::photo_file_name(a.file_name.as_deref(), format, n, false) {
        Ok(name) => Some(name),
        Err(issue) => {
            checked.issue(issue);
            None
        }
    };
    let tags = tags(checked, a.tags);
    checked.call.args["count"] = json!(n);
    checked.call.args["format"] = json!(format);
    let Some(camera) = camera_for(ctx, plan, checked, &a.camera, true) else {
        return;
    };
    let scan = scanner(ctx, &camera);
    if scan {
        checked.warning(
            "count",
            format!("Scans {} pages from the feeder.", grouped(u64::from(n))),
        );
    }
    let Some((output, destination, free_disk)) =
        name.and_then(|name| output(ctx, plan, checked, name, a.destination))
    else {
        return;
    };
    file_fact(ctx, checked, &camera, &output);
    let bytes = photo_estimate(ctx, plan, checked, &camera, n);
    budget(
        ctx,
        plan,
        checked,
        Budget {
            bytes,
            files: u64::from(n),
            exact: false,
            field: "count",
            free_disk,
        },
    );
    checked.summary = format!(
        "Capture {} {} photos from {}.",
        grouped(u64::from(n)),
        format.to_ascii_uppercase(),
        camera.selector()
    );
    checked.ops.push(Op::Command {
        camera,
        command: SessionCommand::Capture {
            output,
            count: n,
            timeout_ms: if scan { 60000 } else { 10000 },
            format: format.into(),
            storage: ctx.storage.request.clone(),
            destination,
            on_exists: OnExists::Number,
            context: tags,
        },
    });
}
fn forwarding(
    ctx: &Context,
    plan: &PlanState,
    checked: &mut Checked,
    camera: &CameraRef,
    codec: Option<String>,
    fps: Option<f64>,
    bitrate: Option<String>,
) -> (String, f64, String) {
    if plan.is_forwarding(camera.selector()) {
        checked.error(
            "camera",
            format!(
                "{} is already recording or streaming. Add a stop step first.",
                camera.selector()
            ),
        );
    }
    if !ctx.media.available {
        checked.issue(Issue::error(
            Some("codec"),
            "Install FFmpeg before recording or streaming video.",
            IssueCode::Unavailable,
        ));
    }
    let codec = codec.unwrap_or_else(|| ctx.defaults.codec.clone());
    let codec = if codec == "h265" {
        "hevc".into()
    } else {
        codec
    };
    if !ctx
        .media
        .encoders_by_codec
        .get(&codec)
        .is_some_and(|e| !e.is_empty())
    {
        checked.issue(Issue::error(
            Some("codec"),
            format!("Choose a codec with an available FFmpeg encoder; {codec} has none."),
            IssueCode::Unavailable,
        ));
    }
    let fps = fps.unwrap_or(ctx.defaults.fps);
    let bitrate = bitrate.unwrap_or_else(|| ctx.defaults.bitrate.clone());
    if !fps.is_finite() || !(0.1..=240.0).contains(&fps) {
        checked.error("fps", "Enter a frame rate from 0.1 to 240.");
    }
    if let Err(error) = crate::media::validate_bitrate(&bitrate) {
        checked.error("bitrate", sentence(&error.to_string()));
    }
    if let Some(c) = ctx
        .snapshot
        .cameras
        .iter()
        .find(|c| c.info.id == camera.selector())
        && c.streaming
        && c.fps > 0.0
        && fps > c.fps * 1.1
    {
        checked.warning("fps", format!("{} delivers about {:.0} fps; at {fps} fps frames repeat and the video is not smoother.", camera_name(ctx, &c.info), c.fps));
    }
    checked.call.args["codec"] = json!(codec);
    checked.call.args["fps"] = json!(fps);
    checked.call.args["bitrate"] = json!(bitrate);
    (codec, fps, bitrate)
}
fn recording(ctx: &Context, plan: &mut PlanState, checked: &mut Checked, a: Recording) {
    let tags = tags(checked, a.tags);
    let Some(camera) = camera_for(ctx, plan, checked, &a.camera, true) else {
        return;
    };
    if scanner(ctx, &camera) {
        checked.issue(Issue::error(
            Some("camera"),
            "Choose a video camera; a document scanner cannot record video.",
            IssueCode::Unsupported,
        ));
    }
    let (codec, fps, bitrate) = forwarding(ctx, plan, checked, &camera, a.codec, a.fps, a.bitrate);
    let explicit = a.duration.is_some();
    let duration_ms = match a.duration {
        Some(text) => duration(checked, "duration", &text, 1000),
        None => crate::media::max_duration_for(&bitrate, crate::media::default_recording_cap())
            .map_err(|e| checked.error("bitrate", sentence(&e.to_string())))
            .ok(),
    };
    let Some(duration_ms) = duration_ms else {
        return;
    };
    if duration_ms == 0 {
        checked.error(
            "bitrate",
            "Lower the bitrate so at least one minute fits in the 512 MiB recording limit, or set a duration.",
        );
        return;
    }
    if !explicit {
        let minutes = duration_ms / 60_000;
        checked.warning("duration", format!("Without a duration this recording stops after about {minutes} minutes. Set a duration to record longer."));
        checked.facts.push((
            "Stops after".into(),
            format!("≈ {} (512 MiB limit)", format_duration_field(duration_ms)),
        ));
    }
    let reservation = if explicit {
        match crate::media::recording_reservation(&bitrate, duration_ms) {
            Ok(bytes) => bytes,
            Err(error) => {
                checked.error("duration", sentence(&error.to_string()));
                return;
            }
        }
    } else {
        crate::media::default_recording_cap()
    };
    if let Some(issue) = values::awake_warning(ctx, ctx.now_ms.saturating_add(duration_ms)) {
        checked.issue(issue);
    }
    let name = match values::recording_file_name(a.file_name.as_deref(), &codec) {
        Ok(name) => name,
        Err(issue) => {
            checked.issue(issue);
            return;
        }
    };
    let Some((output, destination, free_disk)) = output(ctx, plan, checked, name, a.destination)
    else {
        return;
    };
    file_fact(ctx, checked, &camera, &output);
    budget(
        ctx,
        plan,
        checked,
        Budget {
            bytes: reservation,
            files: 1,
            exact: true,
            field: "duration",
            free_disk,
        },
    );
    checked.summary = format!(
        "Record {} for {}.",
        camera.selector(),
        format_duration_field(duration_ms)
    );
    checked.ops.push(Op::Command {
        camera: camera.clone(),
        command: SessionCommand::Forward {
            output,
            codec,
            encoder: "auto".into(),
            fps,
            bitrate,
            storage: ctx.storage.request.clone(),
            max_file_bytes: reservation,
            destination,
            on_exists: OnExists::Number,
            context: tags,
            duration_ms: Some(duration_ms),
        },
    });
    if checked.ready() {
        plan.forwarding.insert(camera.selector().into());
        plan.forwarding_ids.insert(camera.selector().into(), None);
    }
}
fn schedule(ctx: &Context, plan: &mut PlanState, checked: &mut Checked, a: Schedule) {
    let interval = duration(checked, "every", &a.every, 1);
    let ways = usize::from(a.duration.is_some())
        + usize::from(a.count.is_some())
        + usize::from(a.until.is_some());
    if ways != 1 {
        checked.error("duration", "Choose only one way to end this schedule.");
    }
    let first = match a.start_at {
        Some(text) => match values::parse_time(ctx, "start_at", &text, None) {
            Ok(t) => {
                checked.call.args["start_at"] = json!(t.canonical);
                checked.issues.extend(t.issues);
                Some(t.unix_ms)
            }
            Err(issue) => {
                checked.issue(issue);
                None
            }
        },
        None => Some(ctx.now_ms),
    };
    let until = match a.until {
        Some(text) => match values::parse_time(ctx, "until", &text, first) {
            Ok(t) => {
                checked.call.args["until"] = json!(t.canonical);
                checked.issues.extend(t.issues);
                Some(t.unix_ms)
            }
            Err(issue) => {
                checked.issue(issue);
                None
            }
        },
        None => None,
    };
    let dur = a
        .duration
        .as_deref()
        .and_then(|text| duration(checked, "duration", text, 1000));
    let n = match (a.count, interval, first) {
        (Some(n), _, _) => Some(count(checked, n, 100000)),
        (_, Some(every), Some(first)) => {
            let count = dur
                .map(|duration| crate::scheduling::count_for(duration, every))
                .or_else(|| until.map(|until| crate::scheduling::count_until(first, every, until)));
            match count {
                Some(Ok(n)) => Some(n),
                Some(Err(error)) => {
                    checked.error(
                        if until.is_some() { "until" } else { "duration" },
                        sentence(&error.to_string()),
                    );
                    None
                }
                None => None,
            }
        }
        _ => None,
    };
    let tags = tags(checked, a.tags);
    let format = a.format.as_deref().unwrap_or("png");
    photo_format(ctx, checked, format);
    checked.call.args["format"] = json!(format);
    let name = match values::photo_file_name(a.file_name.as_deref(), format, n.unwrap_or(1), true) {
        Ok(name) => Some(name),
        Err(issue) => {
            checked.issue(issue);
            None
        }
    };
    let saved_output = name.and_then(|name| output(ctx, plan, checked, name, a.destination));
    let Some(camera) = camera_for(ctx, plan, checked, &a.camera, true) else {
        return;
    };
    if plan.active_jobs(camera.selector()) >= 32 {
        checked.error(
            "camera",
            "Cancel a scheduled capture before adding another; at most 32 may be active.",
        );
    }
    let (Some(n), Some(every), Some(first)) = (n, interval, first) else {
        return;
    };
    let scan = scanner(ctx, &camera);
    if scan {
        checked.warning(
            "count",
            format!("Scans {} pages from the feeder.", grouped(u64::from(n))),
        );
    }
    let Some((output, destination, free_disk)) = saved_output else {
        return;
    };
    file_fact(ctx, checked, &camera, &output);
    let end = first.saturating_add(every.saturating_mul(u64::from(n.saturating_sub(1))));
    if end
        > ctx
            .now_ms
            .saturating_add(crate::scheduling::MAX_DURATION_MS)
    {
        checked.error(
            "count",
            "Choose a schedule ending within the next 10 years.",
        );
    }
    if let Some(issue) = values::awake_warning(ctx, end) {
        checked.issue(issue);
    }
    checked.facts.push((
        "Schedule".into(),
        format!(
            "{} photos · first {} · last {}",
            grouped(u64::from(n)),
            ctx.zone.clock(first),
            ctx.zone.clock(end)
        ),
    ));
    let bytes = photo_estimate(ctx, plan, checked, &camera, n);
    budget(
        ctx,
        plan,
        checked,
        Budget {
            bytes,
            files: u64::from(n),
            exact: false,
            field: if until.is_some() {
                "until"
            } else if dur.is_some() {
                "duration"
            } else {
                "count"
            },
            free_disk,
        },
    );
    checked.summary = format!(
        "Capture {} photos from {} every {}.",
        grouped(u64::from(n)),
        camera.selector(),
        format_duration_field(every)
    );
    checked.ops.push(Op::Command {
        camera: camera.clone(),
        command: SessionCommand::Schedule {
            output,
            count: n,
            timeout_ms: if scan { 60000 } else { 10000 },
            format: format.into(),
            first_at_ms: first,
            interval_ms: every,
            storage: ctx.storage.request.clone(),
            destination,
            on_exists: OnExists::Number,
            context: tags,
        },
    });
    if checked.ready() {
        *plan.jobs.entry(camera.selector().into()).or_default() += 1;
    }
}
fn stream(ctx: &Context, plan: &mut PlanState, checked: &mut Checked, a: Stream) {
    let Some(camera) = camera_for(ctx, plan, checked, &a.camera, false) else {
        return;
    };
    if scanner(ctx, &camera) {
        checked.issue(Issue::error(
            Some("camera"),
            "Choose a video camera; a document scanner cannot stream video.",
            IssueCode::Unsupported,
        ));
    }
    let (codec, fps, bitrate) = forwarding(ctx, plan, checked, &camera, a.codec, a.fps, a.bitrate);
    let url = a.url.trim();
    let url = url.split_once("://").map_or_else(
        || url.to_owned(),
        |(scheme, rest)| format!("{}://{rest}", scheme.to_ascii_lowercase()),
    );
    let safe_url = super::redact::for_output_text(&url);
    let (has_password, without_userinfo) = url.split_once("://").map_or_else(
        || (false, url.clone()),
        |(scheme, rest)| {
            let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
            authority.rsplit_once('@').map_or_else(
                || (false, url.clone()),
                |(userinfo, host)| {
                    (
                        userinfo.contains(':'),
                        format!("{scheme}://{host}{}", &rest[authority.len()..]),
                    )
                },
            )
        },
    );
    // A username is needed when the executor merges a separately supplied
    // password. Check credential queries and RTMP keys without that username;
    // display redaction deliberately hides all userinfo, including usernames.
    let has_credentials =
        has_password || super::redact::for_output_text(&without_userinfo) != without_userinfo;
    checked.call.args["url"] = json!(if has_credentials { &safe_url } else { &url });
    if !["rtsp://", "rtsps://", "rtmp://", "rtmps://", "srt://"]
        .iter()
        .any(|scheme| url.starts_with(scheme))
    {
        checked.error("url", "Enter an RTSP, RTMP or SRT destination URL.");
    }
    if has_credentials {
        checked.error(
            "url",
            "Remove the password from the URL and enter it in Stream password.",
        );
    }
    if let Err(error) = crate::media::validate_network_url(&url, "Stream URL") {
        checked.error("url", sentence(&error.to_string()));
    }
    let duration_ms = a
        .duration
        .as_deref()
        .and_then(|text| duration(checked, "duration", text, 1000));
    let host = safe_url
        .split_once("://")
        .map(|(_, rest)| rest.split(['/', '?', '#']).next().unwrap_or(rest))
        .unwrap_or(&safe_url);
    let host = host.rsplit_once('@').map_or(host, |(_, host)| host);
    checked.leaves_computer = true;
    checked.warning(
        "url",
        format!(
            "Sends video to {host}.{}",
            if url.starts_with("rtsp://") || url.starts_with("rtmp://") || url.starts_with("srt://")
            {
                " Unencrypted."
            } else {
                ""
            }
        ),
    );
    checked.summary = format!("Stream {} to {host}.", camera.selector());
    // A refused URL must never linger in an operation's Debug representation.
    if has_credentials {
        return;
    }
    checked.ops.push(Op::Stream {
        camera: camera.clone(),
        command: SessionCommand::Forward {
            output: url,
            codec,
            encoder: "auto".into(),
            fps,
            bitrate,
            storage: ctx.storage.request.clone(),
            max_file_bytes: crate::media::default_recording_cap(),
            destination: None,
            on_exists: OnExists::Number,
            context: Default::default(),
            duration_ms,
        },
    });
    if checked.ready() {
        plan.forwarding.insert(camera.selector().into());
        plan.forwarding_ids.insert(camera.selector().into(), None);
    }
}

#[cfg(test)]
#[path = "capture_tests.rs"]
mod tests;
