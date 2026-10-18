//! Observation tools. They read the session and this computer and never
//! change either: no Connect, Select, Set, Execute, Start or Stop. The only
//! commands they send are Status, Discover (at most once per user turn) and
//! Frame, whose replies are measurements and a bounded preview.
//!
//! Every result is redacted for its reader and kept under
//! `MAX_OUTPUT_BYTES`, with a marker where lists were cut.
use super::{
    CameraRef, FieldSpec, Issue, IssueCode, MAX_OUTPUT_BYTES, Origin, Risk, ToolCall, ToolKind,
    ToolSpec, camera_name, connected_device,
    context::{Context, ToolHost},
    fields, format_bytes, grouped, parse_args, parse_duration_field, resolve_camera, risk, schema,
};
use crate::{
    auto::AutoStatus,
    genicam::FeatureInfo,
    scheduling::{CaptureJob, format_duration},
    session::{CameraSnapshot, SessionCommand, SessionSnapshot},
    types::{CameraInfo, Transport},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

/// How often `wait_for` reads the session.
pub const WAIT_POLL: Duration = Duration::from_millis(250);
/// The longest `wait_for`.
pub const MAX_WAIT: Duration = Duration::from_secs(600);
/// How long `list_cameras` searches for cameras.
pub const DISCOVER_MS: u64 = 3_000;
/// How long `inspect_frame` waits for its frame.
pub const FRAME_WAIT_MS: u64 = 5_000;
/// `inspect_frame` image sizes: 0, or a longest edge in this range. Larger
/// images cost more and vision models downscale them anyway.
pub const MIN_EDGE: i64 = 256;
pub const MAX_EDGE: i64 = 1_568;

const fn observe(
    name: &'static str,
    title: &'static str,
    needs_session: bool,
    description: &'static str,
    schema: fn() -> Value,
    fields: &'static [FieldSpec],
) -> ToolSpec {
    ToolSpec {
        name,
        title,
        kind: ToolKind::Read,
        risk: Risk::Observe,
        needs_session,
        needs_vision: false,
        description,
        schema,
        fields,
    }
}

pub const GET_OVERVIEW: ToolSpec = observe(
    "get_overview",
    "Get overview",
    false,
    "Summarize this Capturefab session and computer: the local time and UTC offset, whether a \
     session is running, connected cameras (ID, name, kind, live view, recording, jobs, auto \
     exposure, last error), cameras found by the last discovery, saved destinations, the \
     storage budget and its use, uploads, recording defaults (codec, fps, bitrate) and FFmpeg \
     encoders. Call this first, and use only the camera IDs, names and destination names it \
     and the other tools return. Works without a session. Read-only.",
    schema::no_arguments,
    &[],
);

pub const LIST_CAMERAS: ToolSpec = observe(
    "list_cameras",
    "List cameras",
    false,
    "List connected cameras with their state (live view, frame rate, size, pixel format, \
     recording, jobs, auto exposure, the change undo would revert) and the cameras found by \
     discovery that are not connected. discover true searches the network and this computer \
     for about 3 s and needs a running session; it runs at most once per request, and later \
     calls reuse its result. include_simulated also lists simulator cameras (sim:N), which make \
     synthetic test images. Webcams are listed by their own names. Read-only: never connects a \
     camera.",
    list_cameras_schema,
    fields::LIST_CAMERAS,
);

pub const LIST_DESTINATIONS: ToolSpec = observe(
    "list_destinations",
    "List destinations",
    false,
    "List the saved destinations captures and recordings can go to: name, kind (folder, s3, \
     ftp or sftp), location, the server files are sent to, whether they leave this computer or \
     travel unencrypted, free space and any problem such as an unmounted drive. A null \
     destination saves to the default capture folder, listed as default. Read-only.",
    schema::no_arguments,
    &[],
);

pub const GET_STORAGE: ToolSpec = observe(
    "get_storage",
    "Get storage",
    false,
    "Report the storage budget captures and recordings must fit in (bytes, files, maximum age, \
     and whether a full budget stops captures or deletes the oldest), what is used and \
     reserved, what is left, free disk space at the capture folder, and the upload queue \
     (pending, failed, last error). Read-only.",
    schema::no_arguments,
    &[],
);

pub const LIST_JOBS: ToolSpec = observe(
    "list_jobs",
    "List jobs",
    true,
    "List work running in the background: scheduled captures (job ID, status, progress, \
     interval, next and last capture times, file names, last file, error) and recordings or \
     streams (target, start and end times). camera null lists every camera. Running recordings \
     and schedules cannot be edited: stop or cancel them and start new ones. Use job IDs with \
     wait_for and cancel_job. Read-only.",
    list_jobs_schema,
    fields::LIST_JOBS,
);

pub const GET_CAMERA_FEATURES: ToolSpec = observe(
    "get_camera_features",
    "Get camera features",
    true,
    "Read a connected camera's settings. With search and names null it returns the imaging \
     controls the camera has: exposure, gain, white balance, gamma, black level, pixel format, \
     size and offsets, binning, decimation, frame rate, trigger, focus, zoom, iris, lighting \
     and webcam video mode. search matches part of a name; names reads exact features. Each \
     has value, unit, min, max, inc, choices, writable, locked_while_streaming (change only \
     while live view is stopped) and managed_by_auto (setting it turns auto exposure off). \
     Capturefab manages transport, network, user set and file features; they are never \
     listed. Webcams have no exposure, gain or white balance controls. Read-only.",
    get_camera_features_schema,
    fields::GET_CAMERA_FEATURES,
);

pub const INSPECT_FRAME: ToolSpec = observe(
    "inspect_frame",
    "Inspect frame",
    true,
    "Measure a connected camera's newest frame: exposure (mean, percentiles, clipped \
     highlights, crushed shadows, histogram, colour means), the auto exposure meter, focus \
     scores and notes, with a downscaled image when images are allowed. fresh (0..10, default \
     1) waits for that many new frames; use 2 after changing settings on a real camera. \
     max_dimension (0 or 256..1568, default 1024) is the image's longest edge; 0 sends \
     measurements only. region (x, y, width, height from 0 to 1) crops that part at full \
     resolution and is where focus is measured; use it, not a larger image, to see detail. \
     include_image false skips the image. A stopped camera takes one frame and stays stopped; \
     a triggered camera needs TriggerSoftware. Compare focus scores only for the same region. \
     Never changes settings or saves files.",
    inspect_frame_schema,
    fields::INSPECT_FRAME,
);

pub const WAIT_FOR: ToolSpec = observe(
    "wait_for",
    "Wait",
    true,
    "Wait without using a round, reading the session every 250 ms. until: time waits the \
     whole timeout; frames waits for that many new frames (1..1000) from camera; auto_settled \
     waits until camera's auto exposure is stable or limited; job_done waits until job job_id \
     ends; recording_stopped waits until camera stops recording or streaming; activity waits \
     until someone else uses the session (on camera, or anywhere when camera is null). timeout \
     is a duration from 1s to 10m, such as 30s, 2m or 1m30s. Returns satisfied, waited_ms, \
     reason (met, timeout, human_change or stopped) and the camera's state. Use wait_for \
     instead of calling inspect_frame repeatedly.",
    wait_for_schema,
    fields::WAIT_FOR,
);

fn list_cameras_schema() -> Value {
    schema::object(vec![
        (
            "discover",
            schema::boolean("Search for cameras for about 3 s first (needs a session)."),
        ),
        (
            "include_simulated",
            schema::boolean("Also list simulator cameras (sim:N)."),
        ),
    ])
}

fn list_jobs_schema() -> Value {
    schema::object(vec![(
        "camera",
        schema::nullable(schema::string(
            "A camera ID or name; null lists every camera.",
        )),
    )])
}

fn get_camera_features_schema() -> Value {
    schema::object(vec![
        ("camera", schema::string("A connected camera's ID or name.")),
        (
            "search",
            schema::nullable(schema::string(
                "Part of a feature name, such as Exposure; null for the imaging controls.",
            )),
        ),
        (
            "names",
            schema::nullable(schema::array(
                schema::string("A feature name."),
                "Exact feature names; null for the imaging controls.",
            )),
        ),
    ])
}

fn inspect_frame_schema() -> Value {
    schema::object(vec![
        ("camera", schema::string("A connected camera's ID or name.")),
        (
            "fresh",
            schema::nullable(schema::integer(
                "New frames to wait for, 0..10; null for 1.",
            )),
        ),
        (
            "max_dimension",
            schema::nullable(schema::integer(
                "The image's longest edge: 0 for measurements only, else 256..1568; null for 1024.",
            )),
        ),
        (
            "region",
            schema::nullable(schema::described(
                schema::object(vec![
                    ("x", schema::number("Left edge, 0..1.")),
                    ("y", schema::number("Top edge, 0..1.")),
                    (
                        "width",
                        schema::number("Width, above 0, with x + width at most 1."),
                    ),
                    (
                        "height",
                        schema::number("Height, above 0, with y + height at most 1."),
                    ),
                ]),
                "A part of the frame as fractions of its size; null for the whole frame.",
            )),
        ),
        (
            "include_image",
            schema::nullable(schema::boolean(
                "Send the image when images are allowed; null for true.",
            )),
        ),
    ])
}

fn wait_for_schema() -> Value {
    schema::object(vec![
        (
            "camera",
            schema::nullable(schema::string(
                "A connected camera's ID or name; required for frames, auto_settled and recording_stopped.",
            )),
        ),
        (
            "until",
            schema::choice(
                &[
                    "time",
                    "frames",
                    "auto_settled",
                    "job_done",
                    "recording_stopped",
                    "activity",
                ],
                "What to wait for.",
            ),
        ),
        (
            "timeout",
            schema::string("The longest wait, 1s..10m, such as 30s or 2m."),
        ),
        (
            "frames",
            schema::nullable(schema::integer(
                "New frames to wait for, 1..1000, with until frames; else null.",
            )),
        ),
        (
            "job_id",
            schema::nullable(schema::integer(
                "The job to wait for, with until job_done; else null.",
            )),
        ),
    ])
}

/// A tool's result: JSON for the reader, and images to send beside it.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolOutput {
    pub value: Value,
    pub images: Vec<Image>,
}

impl From<Value> for ToolOutput {
    fn from(value: Value) -> Self {
        Self {
            value,
            images: Vec::new(),
        }
    }
}

/// An image a tool returns, never kept in its JSON.
#[derive(Clone, Debug, PartialEq)]
pub struct Image {
    pub mime: &'static str,
    pub data: Arc<[u8]>,
    pub width: u32,
    pub height: u32,
    /// "Image 1: inspect_frame sim:0, frame 4031, 14:02:31, 1024x768 of
    /// sensor [0,0,2048,1536]".
    pub label: String,
}

/// What one user turn has already done, so repeated calls reuse it: a
/// discovery runs at most once per turn, however often the model asks.
/// Its cached discovery includes simulators, which each caller can filter
/// out. Make a new one for each user turn.
#[derive(Debug, Default)]
pub struct TurnCache {
    discovery: Mutex<Option<Result<Value, String>>>,
}

impl TurnCache {
    pub fn new() -> Self {
        Self::default()
    }

    fn discover(
        &self,
        discover: impl FnOnce() -> anyhow::Result<Value>,
    ) -> anyhow::Result<(Value, bool)> {
        let mut cached = self.discovery.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(reply) = cached.as_ref() {
            return reply
                .clone()
                .map(|reply| (reply, true))
                .map_err(anyhow::Error::msg);
        }
        let reply = discover().map_err(|error| format!("{error:#}"));
        *cached = Some(reply.clone());
        reply
            .map(|reply| (reply, false))
            .map_err(anyhow::Error::msg)
    }
}

/// How a caller runs observation tools.
pub struct ReadOptions<'a> {
    /// `inspect_frame` may return an image.
    pub allow_images: bool,
    pub origin: Origin,
    /// Set to stop `wait_for` early.
    pub cancel: &'a AtomicBool,
    /// Asked on every `wait_for` slice: Some(what changed) when a person
    /// changed something the caller watches, which ends the wait.
    pub interrupt: Option<&'a dyn Fn() -> Option<String>>,
    /// The longest `wait_for`, such as the time a task has left.
    pub max_wait: Duration,
    /// This user turn's cache; None discovers on every call.
    pub turn: Option<&'a TurnCache>,
    /// The number the next returned image is labelled with ("Image 3").
    pub image_number: u32,
}

impl<'a> ReadOptions<'a> {
    pub fn new(origin: Origin, cancel: &'a AtomicBool) -> Self {
        Self {
            allow_images: true,
            origin,
            cancel,
            interrupt: None,
            max_wait: MAX_WAIT,
            turn: None,
            image_number: 1,
        }
    }
}

/// Run observation tool `call`. Problems with the call, and failed session
/// commands, come back as issues; the result is redacted for
/// `opts.origin` and bounded.
pub fn run_read(
    host: &dyn ToolHost,
    ctx: &Context,
    call: &ToolCall,
    opts: &ReadOptions,
) -> Result<ToolOutput, Vec<Issue>> {
    let Some(spec) = super::spec(&call.tool).filter(|spec| spec.kind == ToolKind::Read) else {
        return Err(vec![Issue::usage(
            None,
            format!(
                "{} is not an observation tool; list the tools for their names.",
                opts.origin.redact_text(&call.tool)
            ),
        )]);
    };
    if spec.needs_session && !host.has_session() {
        return Err(vec![Issue::usage(None, needs_session(spec.name))]);
    }
    preflight_for(ctx, call, opts.origin, opts.max_wait).map_err(|issues| {
        issues
            .into_iter()
            .map(|issue| Issue {
                message: opts.origin.redact_text(&issue.message),
                ..issue
            })
            .collect::<Vec<_>>()
    })?;
    let output = match spec.name {
        "get_overview" => parse_args::<NoArgs>(spec, &call.args).map(|_| overview(ctx).into()),
        "list_cameras" => parse_args(spec, &call.args)
            .and_then(|args| list_cameras(host, ctx, args, opts))
            .map(ToolOutput::from),
        "list_destinations" => {
            parse_args::<NoArgs>(spec, &call.args).map(|_| destinations(ctx).into())
        }
        "get_storage" => parse_args::<NoArgs>(spec, &call.args).map(|_| storage(ctx).into()),
        "list_jobs" => parse_args(spec, &call.args)
            .and_then(|args| list_jobs(ctx, args, opts.origin))
            .map(ToolOutput::from),
        "get_camera_features" => parse_args(spec, &call.args)
            .and_then(|args| camera_features(host, ctx, args, opts.origin))
            .map(ToolOutput::from),
        "inspect_frame" => {
            parse_args(spec, &call.args).and_then(|args| inspect_frame(host, ctx, args, opts))
        }
        "wait_for" => parse_args(spec, &call.args)
            .and_then(|args| wait_for(host, ctx, args, opts))
            .map(ToolOutput::from),
        name => unreachable!("observation tool {name} has no executor"),
    };
    match output {
        Ok(mut output) => {
            output.value = bound(opts.origin.redact(output.value));
            for image in &mut output.images {
                image.label = opts.origin.redact_text(&image.label);
            }
            Ok(output)
        }
        Err(issues) => Err(issues
            .into_iter()
            .map(|issue| Issue {
                message: opts.origin.redact_text(&issue.message),
                ..issue
            })
            .collect()),
    }
}

/// Check an observation without contacting a host, discovering devices or
/// waiting. Dry runs use the same typed argument and state validation as
/// execution; caller-specific wait limits are applied again by `run_read`.
pub(crate) fn preflight(ctx: &Context, call: &ToolCall) -> Result<(), Vec<Issue>> {
    preflight_for(ctx, call, Origin::Cli, MAX_WAIT)
}

fn preflight_for(
    ctx: &Context,
    call: &ToolCall,
    origin: Origin,
    max_wait: Duration,
) -> Result<(), Vec<Issue>> {
    let Some(spec) = super::spec(&call.tool).filter(|spec| spec.kind == ToolKind::Read) else {
        return Err(vec![Issue::usage(
            None,
            "Choose an observation tool from tools list.",
        )]);
    };
    if spec.needs_session && !ctx.session {
        return Err(vec![Issue::usage(None, needs_session(spec.name))]);
    }
    match spec.name {
        "get_overview" | "list_destinations" | "get_storage" => {
            parse_args::<NoArgs>(spec, &call.args).map(|_| ())
        }
        "list_cameras" => {
            let args: ListCamerasArgs = parse_args(spec, &call.args)?;
            if args.discover && !ctx.session {
                return Err(vec![Issue::usage(
                    Some("discover"),
                    "Open a session to discover cameras.",
                )]);
            }
            Ok(())
        }
        "list_jobs" => {
            let args: ListJobsArgs = parse_args(spec, &call.args)?;
            args.camera.as_deref().map_or(Ok(()), |camera| {
                connected_camera(ctx, "camera", camera, origin)
                    .map(|_| ())
                    .map_err(|issue| vec![issue])
            })
        }
        "get_camera_features" => {
            let args: FeaturesArgs = parse_args(spec, &call.args)?;
            connected_camera(ctx, "camera", &args.camera, origin)
                .map(|_| ())
                .map_err(|issue| vec![issue])
        }
        "inspect_frame" => {
            let args: InspectArgs = parse_args(spec, &call.args)?;
            prepare_inspect(ctx, &args, origin).map(|_| ())
        }
        "wait_for" => {
            let args: WaitArgs = parse_args(spec, &call.args)?;
            let wait = prepare_wait(ctx, &args, origin, max_wait)?;
            validate_wait_state(&ctx.snapshot, args.until, wait.camera.as_deref(), wait.job)
        }
        _ => unreachable!("catalog observation has no preflight"),
    }
}

/// The issue for a tool that needs a session when there is none.
pub fn needs_session(tool: &str) -> String {
    if tool == "inspect_frame" {
        "inspect_frame needs a running session; start one with `capturefab serve --name NAME` \
         (or `gui --name NAME`) and connect the camera there."
            .into()
    } else {
        format!(
            "{tool} needs a running session; open `capturefab gui` or `capturefab serve --name NAME`, then choose --session NAME."
        )
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NoArgs {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListCamerasArgs {
    discover: bool,
    include_simulated: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListJobsArgs {
    camera: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FeaturesArgs {
    camera: String,
    search: Option<String>,
    names: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InspectArgs {
    camera: String,
    fresh: Option<i64>,
    max_dimension: Option<i64>,
    region: Option<RegionArg>,
    include_image: Option<bool>,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegionArg {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitArgs {
    camera: Option<String>,
    until: Until,
    timeout: String,
    frames: Option<i64>,
    job_id: Option<i64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Until {
    Time,
    Frames,
    AutoSettled,
    JobDone,
    RecordingStopped,
    Activity,
}

fn overview(ctx: &Context) -> Value {
    let mut overview = json!({
        "now": {
            "local": ctx.zone.rfc3339(ctx.now_ms),
            "unix_ms": ctx.now_ms,
            "offset": ctx.zone.offset(ctx.now_ms).to_string(),
        },
        "session": ctx.session,
        "cameras": ctx.snapshot.cameras.iter()
            .map(|camera| camera_summary(ctx, camera, false)).collect::<Vec<_>>(),
        "discovered": discovered(ctx, &ctx.snapshot.devices, true),
        "destinations": ctx.destinations,
        "default_folder": default_folder(ctx),
        "storage": storage_summary(ctx),
        "uploads": ctx.uploads,
        "recording_defaults": ctx.defaults,
        "ffmpeg": {
            "available": ctx.media.available,
            "encoders": ctx.media.encoders_by_codec,
            "problem": ctx.media.problem,
        },
    });
    if !ctx.session {
        overview["note"] = json!(
            "No session is running, so no cameras are connected. Open `capturefab gui` or \
             `capturefab serve --name NAME` to use cameras."
        );
    }
    overview
}

fn list_cameras(
    host: &dyn ToolHost,
    ctx: &Context,
    args: ListCamerasArgs,
    opts: &ReadOptions,
) -> Result<Value, Vec<Issue>> {
    let (devices, discovery) = if args.discover {
        if !host.has_session() {
            return Err(vec![Issue::usage(
                Some("discover"),
                "Open a session to discover cameras.",
            )]);
        }
        let discover = || {
            host.execute(
                None,
                SessionCommand::Discover {
                    timeout_ms: DISCOVER_MS,
                    // A cached discovery must also answer a later call
                    // which includes simulators without searching twice.
                    simulated: opts.turn.is_some() || args.include_simulated,
                },
            )
        };
        let (reply, reused) = match opts.turn {
            Some(turn) => turn.discover(discover),
            None => discover().map(|reply| (reply, false)),
        }
        .map_err(|error| vec![Issue::failed(&error, &Value::Null, opts.origin)])?;
        let devices: Vec<CameraInfo> =
            serde_json::from_value(reply["devices"].clone()).unwrap_or_default();
        let mut discovery = json!({"ran": true, "reused": reused, "warnings": reply["warnings"]});
        if reused {
            discovery["note"] =
                json!("Discovery already ran for this request; this is its result.");
        }
        (devices, discovery)
    } else {
        (
            ctx.snapshot.devices.clone(),
            json!({"ran": false, "note": "From the last discovery; pass discover true to search again."}),
        )
    };
    Ok(json!({
        "cameras": ctx.snapshot.cameras.iter()
            .map(|camera| camera_summary(ctx, camera, true)).collect::<Vec<_>>(),
        "discovered": discovered(ctx, &devices, args.include_simulated),
        "discovery": discovery,
    }))
}

fn destinations(ctx: &Context) -> Value {
    json!({
        "default": default_folder(ctx),
        "destinations": ctx.destinations,
    })
}

fn storage(ctx: &Context) -> Value {
    let mut storage = storage_summary(ctx);
    storage["capture_folder"] = json!(super::redact::home_path(&ctx.capture_root));
    storage["uploads"] = json!(ctx.uploads);
    storage
}

fn list_jobs(ctx: &Context, args: ListJobsArgs, origin: Origin) -> Result<Value, Vec<Issue>> {
    let cameras: Vec<&CameraSnapshot> = match &args.camera {
        Some(camera) => vec![connected_camera(ctx, "camera", camera, origin).map_err(|i| vec![i])?],
        None => ctx.snapshot.cameras.iter().collect(),
    };
    let mut schedules = Vec::new();
    let mut recordings = Vec::new();
    for camera in cameras {
        // Active jobs first, then finished ones, newest first.
        let mut jobs: Vec<&CaptureJob> = camera.jobs.iter().collect();
        jobs.sort_by_key(|job| (!job.active(), std::cmp::Reverse(job.id)));
        schedules.extend(
            jobs.into_iter()
                .map(|job| job_summary(ctx, &camera.info.id, job)),
        );
        if let Some(forwarding) = forwarding_summary(ctx, camera) {
            let mut recording = forwarding;
            recording["camera"] = json!(camera.info.id);
            recordings.push(recording);
        }
    }
    Ok(json!({"recordings": recordings, "schedules": schedules}))
}

/// Imaging controls `get_camera_features` returns when asked for none in
/// particular, by exact name.
const CURATED: [&str; 32] = [
    "ExposureAuto",
    "ExposureTime",
    "ExposureTimeAbs",
    "GainAuto",
    "Gain",
    "GainAbs",
    "GainSelector",
    "BalanceWhiteAuto",
    "BalanceRatioSelector",
    "BalanceRatio",
    "Gamma",
    "BlackLevel",
    "PixelFormat",
    "Width",
    "Height",
    "OffsetX",
    "OffsetY",
    "WidthMax",
    "HeightMax",
    "AcquisitionFrameRate",
    "AcquisitionFrameRateEnable",
    "ResultingFrameRate",
    "TriggerMode",
    "TriggerSource",
    "TriggerSelector",
    "TriggerSoftware",
    "LensPosition",
    "IlluminationIntensity",
    "Magnification",
    "VideoMode",
    "ReverseX",
    "ReverseY",
];
/// Curated families, by name prefix.
const CURATED_PREFIXES: [&str; 5] = ["Binning", "Decimation", "Focus", "Zoom", "Iris"];

fn curated(name: &str) -> bool {
    CURATED.contains(&name) || CURATED_PREFIXES.iter().any(|p| name.starts_with(p))
}

/// Hidden from observation: managed features and nodes without a value.
/// Camera commands have their own allowlisted action tool.
fn hidden(feature: &FeatureInfo) -> bool {
    risk::managed_name(&feature.name) || risk::not_a_value(&feature.kind)
}

fn camera_features(
    host: &dyn ToolHost,
    ctx: &Context,
    args: FeaturesArgs,
    origin: Origin,
) -> Result<Value, Vec<Issue>> {
    let camera = connected_camera(ctx, "camera", &args.camera, origin).map_err(|i| vec![i])?;
    let id = &camera.info.id;
    // The camera's own status is newer than the context's.
    let status = host
        .execute(Some(id), SessionCommand::Status)
        .map_err(|error| vec![Issue::failed(&error, &json!([args.camera]), origin)])?;
    let features: Vec<FeatureInfo> = serde_json::from_value(status["features"].clone())
        .unwrap_or_else(|_| camera.features.clone());
    let auto: Option<AutoStatus> =
        serde_json::from_value(status["auto"].clone()).unwrap_or_else(|_| camera.auto.clone());
    let streaming = status["streaming"].as_bool().unwrap_or(camera.streaming);
    let entry = |feature: &FeatureInfo| feature_entry(feature, auto.as_ref());
    let mut result = json!({
        "camera": id,
        "name": camera_name(ctx, &camera.info),
        "streaming": streaming,
    });
    match (&args.names, &args.search) {
        (Some(names), _) => {
            let mut found = Vec::new();
            let mut missing = Vec::new();
            let mut managed = Vec::new();
            for name in names {
                match features.iter().find(|f| f.name.eq_ignore_ascii_case(name)) {
                    Some(feature) if hidden(feature) => managed.push(feature.name.clone()),
                    Some(feature) => found.push(entry(feature)),
                    None if risk::managed_name(name) => managed.push(name.clone()),
                    None => missing.push(name.clone()),
                }
            }
            result["features"] = json!(found);
            if !missing.is_empty() {
                result["missing"] = json!(missing);
            }
            if !managed.is_empty() {
                result["managed"] = json!(managed);
                result["managed_note"] = json!(
                    "Capturefab manages these features; change them in the camera inspector."
                );
            }
        }
        (None, Some(search)) => {
            let needle = search.trim().to_ascii_lowercase();
            result["features"] = json!(
                features
                    .iter()
                    .filter(|f| !hidden(f))
                    .filter(|f| {
                        f.name.to_ascii_lowercase().contains(&needle)
                            || f.display_name.to_ascii_lowercase().contains(&needle)
                    })
                    .map(entry)
                    .collect::<Vec<_>>()
            );
        }
        (None, None) => {
            result["features"] = json!(
                features
                    .iter()
                    .filter(|f| !hidden(f) && curated(&f.name))
                    .map(entry)
                    .collect::<Vec<_>>()
            );
        }
    }
    if webcam(&camera.info) {
        result["note"] = json!(
            "Webcams have no exposure, gain or white balance controls: only size, frame rate and \
             video mode change, and only while live view is stopped. Ask a person to change the \
             light."
        );
    }
    Ok(result)
}

fn feature_entry(feature: &FeatureInfo, auto: Option<&AutoStatus>) -> Value {
    let mut entry = json!({
        "name": feature.name,
        "kind": feature.kind,
        "value": feature.value,
        "writable": feature.writable,
        "locked_while_streaming": crate::genicam::stream_locked(&feature.name),
        "managed_by_auto": auto.is_some_and(|auto| auto.managed.contains(&feature.name)),
    });
    for (key, value) in [
        ("unit", json!(feature.unit)),
        ("min", json!(feature.min)),
        ("max", json!(feature.max)),
        ("inc", json!(feature.inc)),
        (
            "error",
            json!(feature.error.as_deref().map(|e| clip(e, 200))),
        ),
    ] {
        if !value.is_null() {
            entry[key] = value;
        }
    }
    if !feature.choices.is_empty() {
        entry["choices"] = json!(feature.choices);
    }
    entry
}

struct PreparedInspect<'a> {
    camera: &'a CameraSnapshot,
    fresh: u32,
    edge: u32,
    region: Option<[f64; 4]>,
}

fn prepare_inspect<'a>(
    ctx: &'a Context,
    args: &InspectArgs,
    origin: Origin,
) -> Result<PreparedInspect<'a>, Vec<Issue>> {
    let mut issues = Vec::new();
    let camera = connected_camera(ctx, "camera", &args.camera, origin)
        .map_err(|issue| issues.push(issue))
        .ok();
    let fresh = args
        .fresh
        .unwrap_or(i64::from(crate::inspect::default_fresh()));
    if !(0..=i64::from(crate::inspect::MAX_FRESH)).contains(&fresh) {
        issues.push(Issue::usage(
            Some("fresh"),
            "Choose fresh from 0 to 10 frames.",
        ));
    }
    let edge = args
        .max_dimension
        .unwrap_or(i64::from(crate::inspect::default_edge()));
    if edge != 0 && !(MIN_EDGE..=MAX_EDGE).contains(&edge) {
        issues.push(Issue::usage(
            Some("max_dimension"),
            format!(
                "Choose max_dimension 0 for measurements only, or {} to {} pixels.",
                grouped(MIN_EDGE as u64),
                grouped(MAX_EDGE as u64)
            ),
        ));
    }
    let region = args.region.map(|r| [r.x, r.y, r.width, r.height]);
    if let Some(region) = region
        && crate::inspect::validate(0, 1, 0, Some(region)).is_err()
    {
        issues.push(Issue::usage(
            Some("region"),
            "Keep region inside the frame: x, y, width and height from 0 to 1, width and \
             height above 0, and x + width and y + height at most 1.",
        ));
    }
    let Some(camera) = camera.filter(|_| issues.is_empty()) else {
        return Err(issues);
    };
    if camera.info.transport == Transport::Scanner {
        return Err(vec![Issue::usage(
            Some("camera"),
            "A document scanner cannot be inspected without scanning a page; use capture_photos to scan one.",
        )]);
    }
    Ok(PreparedInspect {
        camera,
        fresh: fresh as u32,
        edge: edge as u32,
        region,
    })
}

fn inspect_frame(
    host: &dyn ToolHost,
    ctx: &Context,
    args: InspectArgs,
    opts: &ReadOptions,
) -> Result<ToolOutput, Vec<Issue>> {
    let PreparedInspect {
        camera,
        fresh,
        edge,
        region,
    } = prepare_inspect(ctx, &args, opts.origin)?;
    let include = args.include_image.unwrap_or(true);
    let send = include && opts.allow_images && edge > 0;
    let command = SessionCommand::Frame {
        fresh,
        timeout_ms: FRAME_WAIT_MS,
        max_dimension: if send { edge } else { 0 },
        format: crate::inspect::PreviewFormat::Auto,
        region,
    };
    let id = camera.info.id.clone();
    let mut reply = host
        .execute(Some(&id), command)
        .map_err(|error| vec![Issue::failed(&error, &json!([args.camera]), opts.origin)])?;
    let image = reply
        .as_object_mut()
        .and_then(|fields| fields.remove("image"))
        .unwrap_or(Value::Null);
    reply["name"] = json!(camera_name(ctx, &camera.info));
    let mut images = Vec::new();
    let not_sent = if !include {
        Some("include_image is false")
    } else if !opts.allow_images {
        Some("camera images are off, so only measurements are sent")
    } else if edge == 0 {
        Some("max_dimension is 0")
    } else if image.is_null() {
        Some("this camera's pixel format has no preview")
    } else {
        None
    };
    reply["image"] = match not_sent {
        Some(reason) => json!(format!("not sent: {reason}")),
        None => match decode_image(&image) {
            Some((mime, data)) => {
                let number = opts.image_number;
                let (width, height) = (
                    image["width"].as_u64().unwrap_or(0) as u32,
                    image["height"].as_u64().unwrap_or(0) as u32,
                );
                let r = &reply["region"];
                let label = format!(
                    "Image {number}: inspect_frame {id}, frame {}, {}, {width}x{height} of sensor [{},{},{},{}]",
                    reply["frame"]["seq"],
                    ctx.zone.clock(crate::scheduling::now_ms()),
                    r["x"],
                    r["y"],
                    r["width"],
                    r["height"],
                );
                images.push(Image {
                    mime,
                    data,
                    width,
                    height,
                    label,
                });
                json!(format!("attached as Image {number}"))
            }
            None => json!("not sent: the preview could not be read"),
        },
    };
    Ok(ToolOutput {
        value: reply,
        images,
    })
}

/// A preview's MIME type and bytes, from the `{mime, base64}` of a Frame
/// reply.
fn decode_image(image: &Value) -> Option<(&'static str, Arc<[u8]>)> {
    use base64::Engine;
    let mime = match image["mime"].as_str()? {
        "image/jpeg" => "image/jpeg",
        "image/png" => "image/png",
        _ => return None,
    };
    let data = base64::engine::general_purpose::STANDARD
        .decode(image["base64"].as_str()?)
        .ok()?;
    (!data.is_empty()).then(|| (mime, Arc::from(data)))
}

struct PreparedWait {
    camera: Option<String>,
    timeout: u64,
    frames: u64,
    job: u64,
}

fn prepare_wait(
    ctx: &Context,
    args: &WaitArgs,
    origin: Origin,
    max_wait: Duration,
) -> Result<PreparedWait, Vec<Issue>> {
    let mut issues = Vec::new();
    let timeout = match parse_duration_field(&args.timeout) {
        Ok(ms) if ms < 1_000 => {
            issues.push(Issue::usage(
                Some("timeout"),
                "Choose a timeout of at least 1 s.",
            ));
            0
        }
        Ok(ms) if ms > MAX_WAIT.as_millis() as u64 => {
            issues.push(Issue::usage(
                Some("timeout"),
                format!(
                    "Choose a timeout of at most {}.",
                    format_duration(MAX_WAIT.as_millis() as u64)
                ),
            ));
            0
        }
        Ok(ms) if ms > max_wait.as_millis() as u64 => {
            issues.push(Issue::usage(
                Some("timeout"),
                format!(
                    "Choose a timeout of at most {}, the time this task has left.",
                    format_duration(max_wait.as_millis() as u64)
                ),
            ));
            0
        }
        Ok(ms) => ms,
        Err(message) => {
            issues.push(Issue::usage(Some("timeout"), message));
            0
        }
    };
    let needs_camera = matches!(
        args.until,
        Until::Frames | Until::AutoSettled | Until::RecordingStopped
    );
    let camera = match &args.camera {
        Some(camera) => connected_camera(ctx, "camera", camera, origin)
            .map_err(|issue| issues.push(issue))
            .ok()
            .map(|camera| camera.info.id.clone()),
        None if needs_camera => {
            issues.push(Issue::usage(
                Some("camera"),
                "Choose the camera to watch; this condition needs one.",
            ));
            None
        }
        None => None,
    };
    let frames = match (args.until, args.frames) {
        (Until::Frames, Some(frames)) if (1..=1000).contains(&frames) => frames as u64,
        (Until::Frames, _) => {
            issues.push(Issue::usage(
                Some("frames"),
                "Choose frames from 1 to 1,000 new frames.",
            ));
            0
        }
        (_, Some(_)) => {
            issues.push(Issue::usage(
                Some("frames"),
                "Set frames only when until is frames; use null.",
            ));
            0
        }
        (_, None) => 0,
    };
    let job = match (args.until, args.job_id) {
        (Until::JobDone, Some(id)) if id >= 1 => id as u64,
        (Until::JobDone, _) => {
            issues.push(Issue::usage(
                Some("job_id"),
                "Choose the job to wait for; list_jobs shows job IDs.",
            ));
            0
        }
        (_, Some(_)) => {
            issues.push(Issue::usage(
                Some("job_id"),
                "Set job_id only when until is job_done; use null.",
            ));
            0
        }
        (_, None) => 0,
    };
    if !issues.is_empty() {
        return Err(issues);
    }
    Ok(PreparedWait {
        camera,
        timeout,
        frames,
        job,
    })
}

fn validate_wait_state(
    snapshot: &SessionSnapshot,
    until: Until,
    camera: Option<&str>,
    job: u64,
) -> Result<(), Vec<Issue>> {
    match until {
        Until::Frames if !find(snapshot, camera).is_some_and(|state| state.streaming) => {
            Err(vec![Issue::usage(
                Some("camera"),
                "This camera is not streaming, so no frames arrive; call \
                 start_live_view first, or wait for something else.",
            )])
        }
        Until::JobDone if job_in(snapshot, camera, job).is_none() => Err(vec![Issue::error(
            Some("job_id"),
            format!("There is no job {job}; list_jobs shows job IDs."),
            IssueCode::Unavailable,
        )]),
        _ => Ok(()),
    }
}

fn wait_for(
    host: &dyn ToolHost,
    ctx: &Context,
    args: WaitArgs,
    opts: &ReadOptions,
) -> Result<Value, Vec<Issue>> {
    let PreparedWait {
        camera,
        timeout,
        frames,
        job,
    } = prepare_wait(ctx, &args, opts.origin, opts.max_wait)?;
    let failed =
        |error: anyhow::Error| vec![Issue::failed(&error, &json!([args.camera]), opts.origin)];
    let first = host.snapshot().map_err(failed)?;
    validate_wait_state(&first, args.until, camera.as_deref(), job)?;
    let ours = host.actor_id();
    let mut watch = Watch {
        until: args.until,
        camera: camera.clone(),
        frames_from: 0,
        frames,
        job,
        last_activity: first
            .activity
            .iter()
            .map(|entry| entry.id)
            .max()
            .unwrap_or(0),
        ours,
    };
    if args.until == Until::Frames {
        watch.frames_from = find(&first, camera.as_deref())
            .expect("validated streaming camera")
            .frames;
    }
    let started = Instant::now();
    let deadline = started + Duration::from_millis(timeout);
    let mut latest = first;
    let (satisfied, reason, note) = loop {
        if opts.cancel.load(Ordering::Acquire) {
            break (false, "stopped", Some("The wait was stopped.".to_owned()));
        }
        if let Some(change) = opts.interrupt.and_then(|interrupt| interrupt()) {
            break (false, "human_change", Some(change));
        }
        if let Some((satisfied, note)) = watch.check(ctx, &latest) {
            break (satisfied, if satisfied { "met" } else { "stopped" }, note);
        }
        let now = Instant::now();
        if now >= deadline {
            break if args.until == Until::Time {
                (true, "met", None)
            } else {
                (false, "timeout", None)
            };
        }
        std::thread::sleep(WAIT_POLL.min(deadline - now));
        latest = host.snapshot().map_err(failed)?;
    };
    let mut result = json!({
        "satisfied": satisfied,
        "waited_ms": started.elapsed().as_millis() as u64,
        "reason": reason,
        "state": wait_state(ctx, &latest, camera.as_deref(), &watch),
    });
    if let Some(note) = note {
        result["note"] = json!(note);
    }
    Ok(result)
}

/// What `wait_for` waits for, with the state it started from.
struct Watch {
    until: Until,
    camera: Option<String>,
    frames_from: u64,
    frames: u64,
    job: u64,
    /// The newest activity entry when the wait started.
    last_activity: u64,
    /// The actor this host's own commands are credited to.
    ours: Option<String>,
}

impl Watch {
    /// Some((satisfied, note)) once the wait is over: the condition is met,
    /// or can no longer be met.
    fn check(&self, ctx: &Context, snapshot: &SessionSnapshot) -> Option<(bool, Option<String>)> {
        let camera = find(snapshot, self.camera.as_deref());
        let gone = || {
            Some((
                false,
                Some(format!(
                    "{} is no longer connected.",
                    self.camera.as_deref().unwrap_or("The camera")
                )),
            ))
        };
        match self.until {
            Until::Time => None,
            Until::Frames => match camera {
                None => gone(),
                Some(camera) if camera.frames >= self.frames_from + self.frames => {
                    Some((true, None))
                }
                Some(camera) if !camera.streaming => Some((
                    false,
                    Some("The camera stopped streaming before enough frames arrived.".into()),
                )),
                Some(_) => None,
            },
            Until::AutoSettled => match camera {
                None => gone(),
                Some(camera) => match &camera.auto {
                    None => Some((
                        true,
                        Some(
                            "Auto exposure is off on this camera, so there is nothing to settle."
                                .into(),
                        ),
                    )),
                    Some(auto) if matches!(auto.state.as_str(), "stable" | "limited") => {
                        Some((true, None))
                    }
                    Some(_) => None,
                },
            },
            Until::RecordingStopped => match camera {
                None => Some((
                    true,
                    Some("The camera was disconnected, which stopped it.".into()),
                )),
                Some(camera) if camera.forwarding.is_none() => Some((true, None)),
                Some(_) => None,
            },
            Until::JobDone => match job_in(snapshot, self.camera.as_deref(), self.job) {
                None => Some((true, Some(format!("Job {} is no longer listed.", self.job)))),
                Some(job) if !job.active() => Some((true, None)),
                Some(_) => None,
            },
            Until::Activity => snapshot
                .activity
                .iter()
                .filter(|entry| entry.id > self.last_activity)
                .filter(|entry| {
                    self.ours.is_none()
                        || entry
                            .actor
                            .as_ref()
                            .is_none_or(|actor| Some(&actor.id) != self.ours.as_ref())
                })
                .find(|entry| {
                    self.camera.is_none() || entry.camera.as_deref() == self.camera.as_deref()
                })
                .map(|entry| {
                    let who = entry
                        .actor
                        .as_ref()
                        .map_or_else(|| entry.origin.label().to_owned(), |a| a.name.clone());
                    let on = entry
                        .camera
                        .as_deref()
                        .and_then(|id| snapshot.cameras.iter().find(|c| c.info.id == id))
                        .map(|camera| format!(" on {}", camera_name(ctx, &camera.info)))
                        .unwrap_or_default();
                    (true, Some(format!("{who}: {}{on}", entry.label)))
                }),
        }
    }
}

fn wait_state(
    ctx: &Context,
    snapshot: &SessionSnapshot,
    camera: Option<&str>,
    watch: &Watch,
) -> Value {
    let mut state = match find(snapshot, camera) {
        Some(camera) => json!({
            "camera": camera.info.id,
            "streaming": camera.streaming,
            "fps": round(camera.fps, 1),
            "frames": camera.frames,
            "recording": forwarding_summary(ctx, camera),
            "auto": camera.auto.as_ref().map(auto_summary),
            "active_jobs": camera.jobs.iter().filter(|job| job.active()).count(),
            "last_error": camera.last_error.as_deref().map(|e| clip(e, 300)),
        }),
        None => json!({
            "cameras": snapshot.cameras.len(),
            "activity_revision": snapshot.activity_revision,
        }),
    };
    if watch.until == Until::Frames {
        state["new_frames"] =
            json!(find(snapshot, camera).map_or(0, |c| c.frames.saturating_sub(watch.frames_from)));
    }
    if watch.until == Until::JobDone
        && let Some((owner, job)) = owned_job(snapshot, camera, watch.job)
    {
        state["job"] = job_summary(ctx, owner, job);
    }
    state
}

fn find<'a>(snapshot: &'a SessionSnapshot, camera: Option<&str>) -> Option<&'a CameraSnapshot> {
    let camera = camera?;
    snapshot.cameras.iter().find(|c| c.info.id == camera)
}

fn job_in<'a>(
    snapshot: &'a SessionSnapshot,
    camera: Option<&str>,
    id: u64,
) -> Option<&'a CaptureJob> {
    owned_job(snapshot, camera, id).map(|(_, job)| job)
}

/// Job `id` and the camera it runs on, on `camera` or any camera.
fn owned_job<'a>(
    snapshot: &'a SessionSnapshot,
    camera: Option<&str>,
    id: u64,
) -> Option<(&'a str, &'a CaptureJob)> {
    snapshot
        .cameras
        .iter()
        .filter(|c| camera.is_none_or(|camera| c.info.id == camera))
        .find_map(|c| {
            c.jobs
                .iter()
                .find(|job| job.id == id)
                .map(|job| (c.info.id.as_str(), job))
        })
}

/// The connected camera `text` names, or the issue to ask for it to be
/// connected first.
fn connected_camera<'a>(
    ctx: &'a Context,
    field: &str,
    text: &str,
    origin: Origin,
) -> Result<&'a CameraSnapshot, Issue> {
    let camera = resolve_camera(ctx, field, text, origin == Origin::Assistant)?;
    camera
        .connected()
        .and_then(|id| ctx.snapshot.cameras.iter().find(|c| c.info.id == id))
        .ok_or_else(|| {
            Issue::error(
                Some(field),
                match camera {
                    CameraRef::Connected(_) => "This camera is no longer connected.".to_owned(),
                    _ => "Connect the camera first; propose connect_camera.".to_owned(),
                },
                IssueCode::Unavailable,
            )
        })
}

fn webcam(info: &CameraInfo) -> bool {
    info.address
        .as_deref()
        .is_some_and(crate::media::is_host_camera)
}

fn camera_summary(ctx: &Context, camera: &CameraSnapshot, detail: bool) -> Value {
    let info = &camera.info;
    let mut summary = json!({
        "id": info.id,
        "name": camera_name(ctx, info),
        "kind": info.profile.kind,
        "transport": info.transport.to_string(),
        "streaming": camera.streaming,
        "fps": round(camera.fps, 1),
        "recording": forwarding_summary(ctx, camera),
        "active_jobs": camera.jobs.iter().filter(|job| job.active()).count(),
        "auto": camera.auto.as_ref().map(auto_summary),
        "last_error": camera.last_error.as_deref().map(|e| clip(e, 300)),
    });
    if webcam(info) {
        summary["webcam"] = json!(true);
    }
    if info.transport == Transport::Scanner {
        summary["scanner"] = json!(true);
    }
    if detail {
        let value = |name: &str| {
            camera
                .features
                .iter()
                .find(|f| f.name == name)
                .and_then(|f| f.value.clone())
        };
        if let (Some(width), Some(height)) = (value("Width"), value("Height")) {
            summary["size"] = json!(format!("{width}x{height}"));
        }
        summary["pixel_format"] = json!(value("PixelFormat"));
        summary["vendor"] = json!(info.vendor);
        summary["model"] = json!(info.model);
        summary["serial"] = json!(info.serial);
        summary["address"] = json!(info.address);
        summary["frames"] = json!(camera.frames);
        summary["dropped"] = json!(camera.dropped);
        summary["undo"] = json!(camera.history.undo);
    }
    summary
}

/// Discovered `devices` that are not connected.
fn discovered(ctx: &Context, devices: &[CameraInfo], simulated: bool) -> Vec<Value> {
    devices
        .iter()
        .filter(|device| connected_device(ctx, device).is_none())
        .filter(|device| simulated || device.transport != Transport::Simulator)
        .map(|device| {
            let mut summary = json!({
                "id": device.id,
                "name": device.model,
                "vendor": device.vendor,
                "kind": device.profile.kind,
                "transport": device.transport.to_string(),
                "serial": device.serial,
                "address": device.address,
            });
            if crate::media::is_host_camera(&device.id) {
                summary["webcam"] = json!(true);
            }
            summary
        })
        .collect()
}

/// A camera's recording or stream, or None.
fn forwarding_summary(ctx: &Context, camera: &CameraSnapshot) -> Option<Value> {
    let target = camera.forwarding.as_deref()?;
    let mut summary = json!({
        "kind": if crate::media::is_recording(target) { "recording" } else { "stream" },
        "to": target,
    });
    if let Some(since) = camera.forwarding_since_ms {
        summary["since"] = json!(ctx.zone.rfc3339(since));
        summary["since_ms"] = json!(since);
    }
    match camera.forwarding_until_ms {
        Some(until) => {
            summary["until"] = json!(ctx.zone.rfc3339(until));
            summary["until_ms"] = json!(until);
            summary["ends_in"] = json!(format_duration(until.saturating_sub(ctx.now_ms)));
        }
        None => summary["until"] = json!("when stopped"),
    }
    Some(summary)
}

fn job_summary(ctx: &Context, camera: &str, job: &CaptureJob) -> Value {
    let last_at = job
        .interval_ms
        .checked_mul(u64::from(job.count.saturating_sub(1)))
        .and_then(|span| job.first_at_ms.checked_add(span));
    let mut summary = json!({
        "camera": camera,
        "id": job.id,
        "status": job.status,
        "captured": job.captured,
        "count": job.count,
        "every": format_duration(job.interval_ms),
        "first_at": ctx.zone.rfc3339(job.first_at_ms),
        "last_at": last_at.map(|at| ctx.zone.rfc3339(at)),
        "file_names": job.output,
        "destination": job.destination,
        "last_file": job.last_file,
        "error": job.error.as_deref().map(|e| clip(e, 300)),
    });
    if job.active() {
        summary["next_at"] = json!(ctx.zone.rfc3339(job.next_at_ms));
    }
    summary
}

fn auto_summary(auto: &AutoStatus) -> Value {
    json!({
        "state": auto.state,
        "strategy": auto.strategy,
        "balance": auto.balance,
        "exposure_us": auto.exposure_us.map(|v| round(v, 1)),
        "gain_db": auto.gain_db.map(|v| round(v, 2)),
        "brightness": auto.brightness.map(|v| round(v, 3)),
        "managed": auto.managed,
    })
}

fn default_folder(ctx: &Context) -> Value {
    json!({
        "location": super::redact::home_path(&ctx.capture_root),
        "free_bytes": ctx.storage.free_disk,
        "note": "A null destination saves here.",
    })
}

fn storage_summary(ctx: &Context) -> Value {
    let storage = &ctx.storage;
    let budget = &storage.effective;
    let mut summary = format!(
        "{} used and {} reserved of a {} budget, so {} is left",
        format_bytes(storage.allocated_bytes),
        format_bytes(storage.reserved_bytes),
        format_bytes(budget.max_bytes),
        format_bytes(storage.free_budget()),
    );
    if let Some(free) = storage.free_disk {
        summary.push_str(&format!("; {} free on disk", format_bytes(free)));
    }
    summary.push('.');
    json!({
        "budget": budget,
        "used_bytes": storage.allocated_bytes,
        "reserved_bytes": storage.reserved_bytes,
        "free_bytes": storage.free_budget(),
        "files": storage.files,
        "free_files": u64::from(budget.max_files).saturating_sub(storage.files),
        "free_disk_bytes": storage.free_disk,
        "summary": summary,
        "when_full": if budget.on_full == "delete-oldest" {
            "The oldest captures are deleted permanently to make room."
        } else {
            "New captures and recordings stop."
        },
    })
}

fn round(value: f64, places: i32) -> f64 {
    let scale = 10f64.powi(places);
    (value * scale).round() / scale
}

/// `text` cut to at most `max` characters, with an ellipsis.
fn clip(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_owned(),
    }
}

/// `value` cut to `MAX_OUTPUT_BYTES` of compact JSON: the largest lists
/// lose their last entries, and a `truncated` note says how many.
pub fn bound(mut value: Value) -> Value {
    // Room for the note itself.
    const LIMIT: usize = MAX_OUTPUT_BYTES - 256;
    let size = |value: &Value| serde_json::to_vec(value).map_or(0, |bytes| bytes.len());
    let mut total = size(&value);
    if total <= MAX_OUTPUT_BYTES {
        return value;
    }
    let mut omitted = 0;
    while total > LIMIT {
        let Some(path) = largest_list(&value) else {
            break;
        };
        let list = pointer_list(&mut value, &path);
        let bytes = size(&Value::Array(list.clone()));
        let average = (bytes / list.len()).max(1);
        let cut = (total - LIMIT).div_ceil(average).clamp(1, list.len());
        list.truncate(list.len() - cut);
        omitted += cut;
        total = size(&value);
    }
    if total > LIMIT || !value.is_object() {
        return json!({
            "truncated": "The result was too large to send; ask for less, for example one camera, a search or names.",
        });
    }
    value["truncated"] = json!(format!(
        "{} entries were left out to keep this result under 24 KiB; ask for less, for example one camera, a search or names.",
        grouped(omitted as u64)
    ));
    value
}

/// The JSON pointer of the non-empty list with the most bytes.
fn largest_list(value: &Value) -> Option<String> {
    fn walk(value: &Value, path: &str, best: &mut Option<(usize, String)>) {
        match value {
            Value::Array(items) => {
                if !items.is_empty() {
                    let bytes = serde_json::to_vec(value).map_or(0, |b| b.len());
                    if best.as_ref().is_none_or(|(most, _)| bytes > *most) {
                        *best = Some((bytes, path.to_owned()));
                    }
                }
                for (index, item) in items.iter().enumerate() {
                    walk(item, &format!("{path}/{index}"), best);
                }
            }
            Value::Object(fields) => {
                for (key, item) in fields {
                    let key = key.replace('~', "~0").replace('/', "~1");
                    walk(item, &format!("{path}/{key}"), best);
                }
            }
            _ => {}
        }
    }
    let mut best = None;
    walk(value, "", &mut best);
    best.map(|(_, path)| path)
}

fn pointer_list<'a>(value: &'a mut Value, path: &str) -> &'a mut Vec<Value> {
    value
        .pointer_mut(path)
        .and_then(Value::as_array_mut)
        .expect("largest_list returns a list's pointer")
}

#[cfg(test)]
mod preflight_tests {
    use super::*;
    use crate::tools::tests::{SCANNER, fixture_context, fixture_snapshot};

    fn call(tool: &str, args: Value) -> ToolCall {
        ToolCall {
            tool: tool.into(),
            args,
        }
    }

    fn wait(camera: Option<&str>, until: &str) -> Value {
        json!({"camera":camera,"until":until,"timeout":"1s","frames":null,"job_id":null})
    }

    #[test]
    fn every_observation_has_a_pure_preflight() {
        let ctx = fixture_context(fixture_snapshot());
        let calls = [
            call("get_overview", json!({})),
            call(
                "list_cameras",
                json!({"discover":true,"include_simulated":false}),
            ),
            call("list_destinations", json!({})),
            call("get_storage", json!({})),
            call("list_jobs", json!({"camera":"sim:0"})),
            call(
                "get_camera_features",
                json!({"camera":"sim:0","search":null,"names":null}),
            ),
            call(
                "inspect_frame",
                json!({"camera":"sim:0","fresh":null,"max_dimension":null,"region":null,"include_image":null}),
            ),
            call("wait_for", wait(None, "time")),
        ];
        for call in calls {
            assert!(preflight(&ctx, &call).is_ok(), "{}", call.tool);
        }
    }

    #[test]
    fn inspection_preflight_checks_every_bound_and_refuses_scanners() {
        let ctx = fixture_context(fixture_snapshot());
        let invalid = call(
            "inspect_frame",
            json!({
                "camera":"sim:0", "fresh":11, "max_dimension":255,
                "region":{"x":0.9,"y":0.0,"width":0.2,"height":1.0}, "include_image":null,
            }),
        );
        let issues = preflight(&ctx, &invalid).unwrap_err();
        for field in ["fresh", "max_dimension", "region"] {
            assert!(
                issues
                    .iter()
                    .any(|issue| issue.field.as_deref() == Some(field)),
                "{field}"
            );
        }
        let scanner = call(
            "inspect_frame",
            json!({"camera":SCANNER,"fresh":null,"max_dimension":null,"region":null,"include_image":null}),
        );
        assert!(
            preflight(&ctx, &scanner).unwrap_err()[0]
                .message
                .contains("document scanner")
        );
    }

    #[test]
    fn wait_preflight_checks_conditions_and_live_state_without_waiting() {
        let ctx = fixture_context(fixture_snapshot());
        for (args, field) in [
            (wait(None, "frames"), "camera"),
            (
                {
                    let mut args = wait(Some("sim:0"), "frames");
                    args["frames"] = json!(1001);
                    args
                },
                "frames",
            ),
            (
                {
                    let mut args = wait(None, "job_done");
                    args["job_id"] = json!(99);
                    args
                },
                "job_id",
            ),
            (
                {
                    let mut args = wait(None, "time");
                    args["timeout"] = json!("11m");
                    args
                },
                "timeout",
            ),
            (
                {
                    let mut args = wait(Some("FaceTime HD Camera"), "frames");
                    args["frames"] = json!(1);
                    args
                },
                "camera",
            ),
        ] {
            let issues = preflight(&ctx, &call("wait_for", args)).unwrap_err();
            assert!(
                issues
                    .iter()
                    .any(|issue| issue.field.as_deref() == Some(field)),
                "{field}: {issues:?}"
            );
        }
        let mut args = wait(Some("sim:0"), "job_done");
        args["job_id"] = json!(7);
        assert!(preflight(&ctx, &call("wait_for", args)).is_ok());
    }

    #[test]
    fn preflight_checks_session_and_camera_requirements() {
        let mut ctx = fixture_context(fixture_snapshot());
        let camera = call(
            "get_camera_features",
            json!({"camera":"gige:192.168.1.20","search":null,"names":null}),
        );
        assert_eq!(
            preflight(&ctx, &camera).unwrap_err()[0].message,
            "Connect the camera first; propose connect_camera."
        );
        ctx.session = false;
        assert!(preflight(&ctx, &call("get_overview", json!({}))).is_ok());
        let discover = call(
            "list_cameras",
            json!({"discover":true,"include_simulated":false}),
        );
        assert_eq!(
            preflight(&ctx, &discover).unwrap_err()[0].message,
            "Open a session to discover cameras."
        );
        assert!(preflight(&ctx, &call("list_jobs", json!({"camera":null}))).is_err());
    }
}
