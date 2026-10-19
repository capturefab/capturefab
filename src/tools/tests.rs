//! `FakeHost`, context fixtures and the tools' tests. No test here touches
//! a camera worker, the network, the keychain or FFmpeg.
use super::{
    context::{
        Compiled, Context, DestinationInfo, MediaInfo, NoSession, RecordingDefaults, SessionHost,
        Sources, StorageInfo, ToolHost, UploadInfo, Zone, gather_with, snapshot_from,
    },
    read::{ReadOptions, ToolOutput, TurnCache, run_read},
    *,
};
use crate::{
    auto::AutoStatus,
    destination::{Bucket, Credentials, Destination, Target},
    genicam::FeatureInfo,
    scheduling::CaptureJob,
    session::{
        ActivityOrigin, ActorKind, CameraHistoryStatus, CameraSnapshot, SessionActivity,
        SessionActor, SessionCommand, SessionHandle, SessionSnapshot,
    },
    storage::StoragePolicy,
    types::{CameraInfo, Transport},
};
use anyhow::Result;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

/// 2026-10-18T12:00:00Z; fixtures show it at +02:00.
pub(crate) const NOW: u64 = 1_792_324_800_000;
pub(crate) const WEBCAM: &str = "media:0123456789abcdef";
pub(crate) const STREAM: &str = "media:fedcba9876543210";
pub(crate) const SCANNER: &str = "scanner:ica:office";
pub(crate) const FACETIME: &str = "avfoundation:FaceTime HD Camera";
pub(crate) const ONVIF: &str = "onvif:http://10.0.0.5/onvif/device_service";

/// A local receipt fixture, without touching a profile file or secret store.
pub(crate) fn fixture_bound_source(ctx: &mut Context, endpoint: &str) -> context::DataSourceRef {
    use crate::signals::auth::{AuthBinding, BindingReceipt, ProfileInfo, ProfileRef, SecretKind};
    let receipt = BindingReceipt {
        endpoint: endpoint.into(),
        reference: crate::signals::source_id(endpoint),
        auth: AuthBinding {
            profile: ProfileRef {
                id: "a".repeat(32),
                revision: "b".repeat(32),
            },
            epoch: "c".repeat(32),
        },
    };
    ctx.data_profiles.push(ProfileInfo {
        profile: receipt.auth.profile.clone(),
        name: "line-one".into(),
        endpoint: endpoint.into(),
        reference: receipt.reference.clone(),
        protocol: crate::signals::parse(endpoint).unwrap().protocol,
        secret: SecretKind::Environment,
        insecure: false,
    });
    ctx.snapshot.signal_bindings.push(receipt.clone());
    context::DataSourceRef::from_binding(&receipt).unwrap()
}

/// A scripted session. `snapshot()` steps through the scripted snapshots
/// and then repeats the last; `execute()` records each command and answers
/// with a queued reply for its op, or a default: Status from the current
/// snapshot (camera-addressed like the coordinator's), Discover with its
/// devices, Connect with a webcam-style ID, and `{}` otherwise.
pub(crate) struct FakeHost {
    snapshots: Vec<SessionSnapshot>,
    next: AtomicUsize,
    replies: Mutex<Vec<(String, Result<Value, String>)>>,
    calls: Mutex<Vec<(Option<String>, SessionCommand)>>,
    session: bool,
    actor: Option<String>,
}

impl FakeHost {
    pub(crate) fn new(snapshot: SessionSnapshot) -> Self {
        Self {
            snapshots: vec![snapshot],
            next: AtomicUsize::new(0),
            replies: Mutex::new(Vec::new()),
            calls: Mutex::new(Vec::new()),
            session: true,
            actor: None,
        }
    }

    /// Answer later `snapshot()` calls with `snapshot`.
    pub(crate) fn then(mut self, snapshot: SessionSnapshot) -> Self {
        self.snapshots.push(snapshot);
        self
    }

    /// Answer the next command whose op is `op` with `reply`.
    pub(crate) fn reply(self, op: &str, reply: Result<Value, String>) -> Self {
        self.replies.lock().unwrap().push((op.to_owned(), reply));
        self
    }

    pub(crate) fn without_session(mut self) -> Self {
        self.session = false;
        self
    }

    pub(crate) fn with_actor(mut self, id: &str) -> Self {
        self.actor = Some(id.to_owned());
        self
    }

    pub(crate) fn calls(&self) -> Vec<(Option<String>, SessionCommand)> {
        self.calls.lock().unwrap().clone()
    }

    /// The ops of every command sent, in order.
    pub(crate) fn ops(&self) -> Vec<String> {
        self.calls()
            .iter()
            .map(|(_, command)| op(command))
            .collect()
    }

    /// Every command sent, as its camera and wire JSON.
    pub(crate) fn sent(&self) -> Vec<(Option<String>, Value)> {
        self.calls()
            .into_iter()
            .map(|(camera, command)| (camera, serde_json::to_value(command).unwrap()))
            .collect()
    }

    fn current(&self) -> SessionSnapshot {
        let at = self.next.load(Ordering::SeqCst).saturating_sub(1);
        self.snapshots[at.min(self.snapshots.len() - 1)].clone()
    }
}

fn op(command: &SessionCommand) -> String {
    serde_json::to_value(command).unwrap()["op"]
        .as_str()
        .unwrap()
        .to_owned()
}

impl ToolHost for FakeHost {
    fn snapshot(&self) -> Result<SessionSnapshot> {
        let at = self.next.fetch_add(1, Ordering::SeqCst);
        Ok(self.snapshots[at.min(self.snapshots.len() - 1)].clone())
    }

    fn execute(&self, camera: Option<&str>, command: SessionCommand) -> Result<Value> {
        self.calls
            .lock()
            .unwrap()
            .push((camera.map(str::to_owned), command.clone()));
        let op = op(&command);
        let mut replies = self.replies.lock().unwrap();
        if let Some(at) = replies.iter().position(|(queued, _)| *queued == op) {
            return replies.remove(at).1.map_err(anyhow::Error::msg);
        }
        drop(replies);
        let current = self.current();
        Ok(match (command, camera) {
            (SessionCommand::Status, Some(camera)) => {
                let mut view = current;
                view.select_view(Some(camera));
                serde_json::to_value(view)?
            }
            (SessionCommand::Status, None) => serde_json::to_value(current)?,
            (SessionCommand::Discover { .. }, _) => {
                json!({"devices": current.devices, "warnings": []})
            }
            (SessionCommand::Connect { .. }, _) => {
                json!({"connected": {"id": "media:00000000000000aa"}, "worker_pid": 1})
            }
            _ => json!({}),
        })
    }

    fn has_session(&self) -> bool {
        self.session
    }

    fn actor_id(&self) -> Option<String> {
        self.actor.clone()
    }
}

pub(crate) fn home() -> PathBuf {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .filter(|home| !home.is_empty())
        .map_or_else(
            || {
                PathBuf::from(if cfg!(windows) {
                    r"C:\home\test"
                } else {
                    "/home/test"
                })
            },
            PathBuf::from,
        )
}

pub(crate) fn feature(name: &str, kind: &str, value: Value) -> FeatureInfo {
    FeatureInfo {
        name: name.into(),
        display_name: name.into(),
        kind: kind.into(),
        value: Some(value),
        writable: kind != "Command",
        description: String::new(),
        unit: None,
        min: None,
        max: None,
        inc: None,
        choices: Vec::new(),
        error: None,
    }
}

pub(crate) fn camera(info: CameraInfo) -> CameraSnapshot {
    CameraSnapshot {
        info,
        features: Arc::default(),
        streaming: false,
        frames: 0,
        received_bytes: 0,
        dropped: 0,
        fps: 0.0,
        last_error: None,
        worker_pid: 1,
        forwarding: None,
        forwarding_id: None,
        forwarding_since_ms: None,
        forwarding_until_ms: None,
        jobs: Vec::new(),
        auto: None,
        capture_session: None,
        transport: None,
        history: CameraHistoryStatus::default(),
        lost: 0,
        preview_skipped: 0,
        stale: false,
        error_logged: None,
    }
}

pub(crate) fn info(
    id: &str,
    transport: Transport,
    model: &str,
    address: Option<&str>,
) -> CameraInfo {
    CameraInfo {
        id: id.into(),
        transport,
        vendor: "Vendor".into(),
        model: model.into(),
        serial: format!("SN-{}", id.len()),
        address: address.map(str::to_owned),
        profile: Default::default(),
    }
}

pub(crate) fn job(id: u64, status: &str) -> CaptureJob {
    CaptureJob {
        id,
        status: status.into(),
        output: "{camera}-{datetime}-{seq}.png".into(),
        count: 288,
        captured: 12,
        skipped: 0,
        retries: 0,
        first_at_ms: NOW - 3_600_000,
        interval_ms: 300_000,
        next_at_ms: NOW + 300_000,
        last_file: Some(format!("{}/Capturefab/sim-0012.png", home().display())),
        error: None,
        destination: None,
        context: Default::default(),
    }
}

/// A session with: sim:0 streaming under converging software auto with a
/// running schedule; a connected FaceTime webcam (`media:…`, address
/// `avfoundation:FaceTime HD Camera`, matching a discovered device); a
/// document scanner; sim:1 recording to a file for 30 more minutes; an RTSP
/// input streaming to an RTMP URL with a stream key. Discovery also found a
/// GigE camera, an ONVIF camera and sim:2.
pub(crate) fn fixture_snapshot() -> SessionSnapshot {
    let mut sim0 = camera(crate::transport::simulator::info_named("sim:0"));
    sim0.streaming = true;
    sim0.fps = 29.97;
    sim0.frames = 1000;
    sim0.features = vec![
        FeatureInfo {
            unit: Some("us".into()),
            min: Some(10.0),
            max: Some(1_000_000.0),
            ..feature("ExposureTime", "Float", json!(10000.0))
        },
        FeatureInfo {
            unit: Some("dB".into()),
            min: Some(0.0),
            max: Some(24.0),
            ..feature("Gain", "Float", json!(0.0))
        },
        FeatureInfo {
            min: Some(64.0),
            max: Some(1920.0),
            inc: Some(2.0),
            ..feature("Width", "Integer", json!(640))
        },
        feature("Height", "Integer", json!(480)),
        FeatureInfo {
            choices: vec!["Mono8".into(), "RGB8".into()],
            ..feature("PixelFormat", "Enumeration", json!("Mono8"))
        },
        feature("AcquisitionFrameRate", "Float", json!(30.0)),
        feature("GevSCDA", "Integer", json!(3232235777u64)),
        feature("TLParamsLocked", "Integer", json!(0)),
        feature("UserSetSave", "Command", Value::Null),
        feature("TriggerSoftware", "Command", Value::Null),
        FeatureInfo {
            writable: false,
            ..feature("DeviceTemperature", "Float", json!(41.5))
        },
        feature("LutValueReg", "IntReg", json!(0)),
    ]
    .into();
    sim0.auto = Some(AutoStatus {
        balance: 0.5,
        strategy: "software".into(),
        state: "converging".into(),
        exposure_us: Some(10_000.0),
        gain_db: Some(0.0),
        brightness: Some(0.31),
        managed: vec!["ExposureTime".into(), "Gain".into()],
        ..AutoStatus::default()
    });
    sim0.jobs = vec![job(7, "running"), job(3, "complete")];

    let mut webcam = camera(info(
        WEBCAM,
        Transport::Media,
        "AVFOUNDATION input",
        Some(FACETIME),
    ));
    webcam.features = vec![
        feature("Width", "Integer", json!(1280)),
        feature("Height", "Integer", json!(720)),
        feature("VideoMode", "Enumeration", json!("1280x720@30")),
    ]
    .into();

    let scanner = camera(CameraInfo {
        vendor: "ICA".into(),
        ..info(
            SCANNER,
            Transport::Scanner,
            "Document scanner",
            Some(SCANNER),
        )
    });

    let mut sim1 = camera(crate::transport::simulator::info_named("sim:1"));
    sim1.streaming = true;
    sim1.forwarding = Some(format!("{}/Capturefab/line.mp4", home().display()));
    sim1.forwarding_id = Some("11111111111111111111111111111111".into());
    sim1.forwarding_since_ms = Some(NOW - 600_000);
    sim1.forwarding_until_ms = Some(NOW + 1_800_000);

    let mut stream = camera(info(
        STREAM,
        Transport::Media,
        "RTSP input",
        Some("rtsp://<redacted>@10.0.0.9/stream"),
    ));
    stream.streaming = true;
    stream.forwarding = Some("rtmp://h/live2/KEY".into());
    stream.forwarding_id = Some("22222222222222222222222222222222".into());
    stream.forwarding_since_ms = Some(NOW - 60_000);
    stream.last_error = Some("could not open rtsp://u:p@h/s: refused".into());

    let mut gige = info(
        "gige:192.168.1.20",
        Transport::GigE,
        "acA1920-40gm",
        Some("192.168.1.20"),
    );
    gige.vendor = "Basler".into();
    SessionSnapshot {
        devices: vec![
            CameraInfo {
                vendor: "OS".into(),
                ..info(FACETIME, Transport::Media, "FaceTime HD Camera", None)
            },
            gige,
            info(
                ONVIF,
                Transport::Media,
                "Lobby camera",
                Some("http://10.0.0.5/onvif/device_service"),
            ),
            crate::transport::simulator::info_named("sim:2"),
        ],
        cameras: vec![sim0, webcam, scanner, sim1, stream],
        active_camera: Some("sim:0".into()),
        ..Default::default()
    }
}

/// A context over `snapshot`, built without I/O.
pub(crate) fn fixture_context(snapshot: SessionSnapshot) -> Context {
    let policy = StoragePolicy::default();
    Context {
        now_ms: NOW,
        zone: Zone::Fixed(7200),
        session: true,
        snapshot,
        data_sources: Vec::new(),
        data_profiles: Vec::new(),
        allow_data: false,
        data_dataset: None,
        task_data_available: false,
        task_data_reserved_bytes: 0,
        destinations: vec![
            DestinationInfo {
                approval_identity: None,
                name: "line-1".into(),
                kind: "folder".into(),
                location: "~/Captures/line-1".into(),
                host: None,
                insecure: false,
                problem: None,
                free_bytes: Some(50_000_000_000),
                remote: false,
            },
            DestinationInfo {
                approval_identity: None,
                name: "archive".into(),
                kind: "s3".into(),
                location: "s3://acme/cams/ at https://minio.example:9000".into(),
                host: Some("minio.example:9000".into()),
                insecure: false,
                problem: None,
                free_bytes: Some(50_000_000_000),
                remote: true,
            },
        ],
        storage: StorageInfo {
            effective: policy.clone(),
            request: policy,
            allocated_bytes: 1_800_000_000,
            reserved_bytes: 0,
            files: 120,
            free_disk: Some(7_100_000_000),
        },
        media: MediaInfo::from_names(&[
            "libx264".into(),
            "h264_videotoolbox".into(),
            "hevc_videotoolbox".into(),
            "aac".into(),
        ]),
        uploads: Some(UploadInfo {
            pending: 1,
            last_error: Some("upload to srt://h?passphrase=x failed".into()),
            ..UploadInfo::default()
        }),
        capture_root: home().join("Capturefab"),
        compiled: Compiled::current(),
        defaults: RecordingDefaults::default(),
    }
}

fn call(tool: &str, args: Value) -> ToolCall {
    ToolCall {
        tool: tool.into(),
        args,
    }
}

fn run(
    host: &dyn ToolHost,
    ctx: &Context,
    tool: &str,
    args: Value,
    opts: &ReadOptions,
) -> Result<ToolOutput, Vec<Issue>> {
    run_read(host, ctx, &call(tool, args), opts)
}

fn messages(issues: &[Issue]) -> Vec<(Option<&str>, &str)> {
    issues
        .iter()
        .map(|issue| (issue.field.as_deref(), issue.message.as_str()))
        .collect()
}

fn size(value: &Value) -> usize {
    serde_json::to_vec(value).unwrap().len()
}

fn assert_clean(text: &str) {
    for secret in ["u:p@", "passphrase=x", "live2/KEY", "hunter2", "AKIA"] {
        assert!(!text.contains(secret), "{secret} leaked: {text}");
    }
}

#[test]
fn catalog_lists_the_observe_tools_with_their_sessions() {
    let names: Vec<_> = catalog()
        .iter()
        .filter(|spec| spec.kind == ToolKind::Read)
        .map(|spec| spec.name)
        .collect();
    assert_eq!(
        names,
        [
            "get_overview",
            "list_cameras",
            "list_destinations",
            "get_storage",
            "list_jobs",
            "get_camera_features",
            "inspect_frame",
            "wait_for",
            "list_data_sources",
            "get_dataset",
            "query_dataset"
        ]
    );
    for spec in catalog().iter().filter(|spec| spec.kind == ToolKind::Read) {
        assert_eq!((spec.kind, spec.risk), (ToolKind::Read, Risk::Observe));
    }
    let offline: Vec<_> = catalog()
        .iter()
        .filter(|spec| !spec.needs_session && spec.kind == ToolKind::Read)
        .map(|spec| spec.name)
        .collect();
    assert_eq!(
        offline,
        [
            "get_overview",
            "list_cameras",
            "list_destinations",
            "get_storage"
        ]
    );
    assert_eq!(spec("inspect_frame").unwrap().title, "Inspect frame");
    assert!(spec("delete_everything").is_none());
    let json = catalog_json();
    assert_eq!(json[6]["risk"], "observe");
    assert_eq!(json[6]["kind"], "read");
    assert_eq!(json[0]["input_schema"]["required"], json!([]));
}

#[test]
fn string_list_form_fields_match_their_array_of_strings_schemas() {
    let mut fields = Vec::new();
    for tool in catalog() {
        let schema = (tool.schema)();
        for field in tool.fields {
            if field.kind == FieldKind::StringList {
                fields.push((tool.name, field.key));
                let schema = &schema["properties"][field.key];
                assert_eq!(schema["items"]["type"], "string", "{fields:?}");
                assert!(schema["type"].as_array().unwrap().contains(&json!("array")));
            }
        }
    }
    assert_eq!(
        fields,
        [
            ("get_camera_features", "names"),
            ("start_signal_capture", "tags"),
            ("export_dataset", "tables"),
        ]
    );
}

#[test]
fn cameras_resolve_by_id_selector_name_and_reduced_id() {
    let ctx = fixture_context(fixture_snapshot());
    let resolve = |text: &str, assistant: bool| resolve_camera(&ctx, "camera", text, assistant);
    assert_eq!(
        resolve("SIM:0", true),
        Ok(CameraRef::Connected("sim:0".into()))
    );
    assert_eq!(
        resolve(FACETIME, true),
        Ok(CameraRef::Connected(WEBCAM.into()))
    );
    assert_eq!(
        resolve("facetime hd camera", true),
        Ok(CameraRef::Connected(WEBCAM.into())),
        "a webcam's name comes from the device its selector names"
    );
    assert_eq!(
        resolve("gige:192.168.1.20", true),
        Ok(CameraRef::Discovered("gige:192.168.1.20".into()))
    );
    assert_eq!(
        resolve("ACA1920-40GM", true),
        Ok(CameraRef::Discovered("gige:192.168.1.20".into()))
    );
    assert_eq!(
        resolve("onvif:http://10.0.0.5", true),
        Ok(CameraRef::Discovered(ONVIF.into())),
        "the ID a model was shown resolves"
    );
    assert_eq!(
        resolve(" Document scanner ", true),
        Ok(CameraRef::Connected(SCANNER.into()))
    );
    let ambiguous = resolve("pattern camera", true).unwrap_err();
    assert_eq!(ambiguous.field.as_deref(), Some("camera"));
    assert_eq!(
        ambiguous.message,
        "More than one camera answers to \"pattern camera\": sim:0, sim:1 and sim:2. Use one camera's ID instead."
    );
    let unknown = resolve("sim:9", true).unwrap_err();
    assert_eq!(
        (unknown.message.as_str(), unknown.code),
        (
            "Choose a camera from the list, or type its address in this field.",
            IssueCode::Unavailable
        )
    );
    assert_eq!(
        resolve("sim:9", false),
        Ok(CameraRef::Typed("sim:9".into()))
    );
    let invalid = resolve("gige:not-an-address", false).unwrap_err();
    assert!(
        invalid
            .message
            .starts_with("GigE selector needs an IPv4 address")
    );
    assert!(invalid.message.ends_with('.'));
    assert_eq!(
        resolve("", false).unwrap_err().field.as_deref(),
        Some("camera")
    );
    assert_eq!(
        camera_name(&ctx, &ctx.snapshot.cameras[1].info),
        "FaceTime HD Camera"
    );
}

#[test]
fn reduced_camera_ids_still_require_an_unambiguous_match() {
    let mut snapshot = fixture_snapshot();
    snapshot.devices.push(info(
        "onvif:http://10.0.0.5/another/device_service",
        Transport::Media,
        "Second lobby camera",
        None,
    ));
    let ctx = fixture_context(snapshot);
    let issue = resolve_camera(&ctx, "camera", "onvif:http://10.0.0.5", true).unwrap_err();
    assert_eq!(issue.field.as_deref(), Some("camera"));
    assert!(issue.message.starts_with("More than one camera answers"));
    assert_eq!(
        resolve_camera(&ctx, "camera", "Second lobby camera", true),
        Ok(CameraRef::Discovered(
            "onvif:http://10.0.0.5/another/device_service".into()
        ))
    );
}

#[test]
fn overview_is_redacted_for_models_and_people() {
    let ctx = fixture_context(fixture_snapshot());
    let host = FakeHost::new(fixture_snapshot());
    let cancel = AtomicBool::new(false);
    let assistant = ReadOptions::new(Origin::Assistant, &cancel);
    let model = run(&host, &ctx, "get_overview", json!({}), &assistant)
        .unwrap()
        .value;
    let text = model.to_string();
    assert_clean(&text);
    assert!(!text.contains(&home().display().to_string()), "{text}");
    assert!(!text.contains("\"serial\"") && !text.contains("\"address\""));
    assert_eq!(model["now"]["offset"], "+02:00");
    assert_eq!(model["now"]["local"], "2026-10-18T14:00:00+02:00");
    assert_eq!(model["cameras"].as_array().unwrap().len(), 5);
    assert_eq!(model["cameras"][1]["name"], "FaceTime HD Camera");
    assert_eq!(model["cameras"][1]["webcam"], true);
    assert_eq!(model["cameras"][3]["recording"]["kind"], "recording");
    assert_eq!(
        model["cameras"][3]["recording"]["to"],
        "~/Capturefab/line.mp4"
    );
    assert_eq!(model["cameras"][3]["recording"]["ends_in"], "30 min");
    assert_eq!(model["cameras"][4]["recording"]["to"], "rtmp://h");
    // The connected webcam is not also listed as discovered.
    let discovered: Vec<_> = model["discovered"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        discovered,
        ["gige:192.168.1.20", "onvif:http://10.0.0.5", "sim:2"]
    );
    assert_eq!(
        model["ffmpeg"]["encoders"]["h264"],
        json!(["libx264", "h264_videotoolbox"])
    );
    assert_eq!(
        model["recording_defaults"],
        json!({"codec": "h264", "fps": 30.0, "bitrate": "4M"})
    );
    assert!(size(&model) <= MAX_OUTPUT_BYTES);
    assert!(
        host.calls().is_empty(),
        "the overview reads the context only"
    );

    let cli = ReadOptions::new(Origin::Cli, &cancel);
    let people = run(&host, &ctx, "get_overview", Value::Null, &cli)
        .unwrap()
        .value;
    assert_clean(&people.to_string());
    assert_eq!(
        people["cameras"][4]["recording"]["to"],
        "rtmp://h/live2/<redacted>"
    );
    assert_eq!(
        people["cameras"][4]["last_error"],
        "could not open rtsp://<redacted>@h/s: refused"
    );
    assert_eq!(
        people["uploads"]["last_error"],
        "upload to srt://h?passphrase=<redacted> failed"
    );
}

#[test]
fn overview_and_offline_tools_work_without_a_session() {
    let mut ctx = fixture_context(SessionSnapshot::default());
    ctx.session = false;
    let cancel = AtomicBool::new(false);
    let opts = ReadOptions::new(Origin::Cli, &cancel);
    let overview = run(&NoSession, &ctx, "get_overview", json!({}), &opts)
        .unwrap()
        .value;
    assert_eq!(overview["session"], false);
    assert!(
        overview["note"]
            .as_str()
            .unwrap()
            .starts_with("No session is running")
    );
    for tool in ["list_destinations", "get_storage"] {
        assert!(
            run(&NoSession, &ctx, tool, json!({}), &opts).is_ok(),
            "{tool}"
        );
    }
    let cameras = run(
        &NoSession,
        &ctx,
        "list_cameras",
        json!({"discover": false, "include_simulated": true}),
        &opts,
    )
    .unwrap()
    .value;
    assert_eq!(cameras["cameras"], json!([]));
    let issues = run(
        &NoSession,
        &ctx,
        "list_cameras",
        json!({"discover": true, "include_simulated": false}),
        &opts,
    )
    .unwrap_err();
    assert_eq!(
        messages(&issues),
        [(Some("discover"), "Open a session to discover cameras.")]
    );
    for (tool, args) in [
        ("list_jobs", json!({"camera": null})),
        (
            "get_camera_features",
            json!({"camera": "sim:0", "search": null, "names": null}),
        ),
        (
            "wait_for",
            json!({"camera": null, "until": "time", "timeout": "1s", "frames": null, "job_id": null}),
        ),
    ] {
        let issues = run(&NoSession, &ctx, tool, args, &opts).unwrap_err();
        assert_eq!(issues[0].code, IssueCode::Usage);
        assert!(
            issues[0]
                .message
                .starts_with(&format!("{tool} needs a running session;")),
            "{issues:?}"
        );
    }
    let issues = run(
        &NoSession,
        &ctx,
        "inspect_frame",
        inspect_args("sim:0"),
        &opts,
    )
    .unwrap_err();
    assert_eq!(
        issues[0].message,
        "inspect_frame needs a running session; start one with `capturefab serve --name NAME` (or `gui --name NAME`) and connect the camera there."
    );
}

#[test]
fn list_cameras_discovers_once_per_turn_and_filters_simulators() {
    let mut snapshot = fixture_snapshot();
    snapshot.cameras[0].dropped = 9;
    snapshot.cameras[0].lost = 2;
    snapshot.cameras[0].preview_skipped = 7;
    let ctx = fixture_context(snapshot);
    let host = FakeHost::new(fixture_snapshot());
    let cancel = AtomicBool::new(false);
    let turn = TurnCache::new();
    let mut opts = ReadOptions::new(Origin::Assistant, &cancel);
    opts.turn = Some(&turn);
    let listed = run(
        &host,
        &ctx,
        "list_cameras",
        json!({"discover": false, "include_simulated": false}),
        &opts,
    )
    .unwrap()
    .value;
    assert!(host.calls().is_empty());
    let ids = |value: &Value, key: &str| -> Vec<String> {
        value[key]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["id"].as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(
        ids(&listed, "discovered"),
        ["gige:192.168.1.20", "onvif:http://10.0.0.5"]
    );
    assert_eq!(ids(&listed, "cameras").len(), 5);
    assert_eq!(listed["cameras"][0]["size"], "640x480");
    assert_eq!(listed["cameras"][0]["pixel_format"], "Mono8");
    assert_eq!(listed["cameras"][0]["dropped"], 9);
    assert_eq!(listed["cameras"][0]["lost"], 2);
    assert_eq!(listed["cameras"][0]["preview_skipped"], 7);
    assert_eq!(listed["discovery"]["ran"], false);
    let args = json!({"discover": true, "include_simulated": true});
    let first = run(&host, &ctx, "list_cameras", args.clone(), &opts)
        .unwrap()
        .value;
    let again = run(&host, &ctx, "list_cameras", args, &opts).unwrap().value;
    assert_eq!(host.ops(), ["discover"], "one discovery per turn");
    assert_eq!(
        host.sent()[0],
        (
            None,
            json!({"op": "discover", "timeout_ms": 3_000, "simulated": true})
        )
    );
    assert_eq!(first["discovery"]["reused"], false);
    assert_eq!(again["discovery"]["reused"], true);
    assert_eq!(
        ids(&again, "discovered"),
        ["gige:192.168.1.20", "onvif:http://10.0.0.5", "sim:2"]
    );
    // A discovery with simulators also answers a call without them.
    let args = json!({"discover": true, "include_simulated": false});
    let real = run(&host, &ctx, "list_cameras", args, &opts).unwrap().value;
    assert_eq!(host.ops(), ["discover"]);
    assert_eq!(real["discovery"]["reused"], true);
    assert_eq!(
        ids(&real, "discovered"),
        ["gige:192.168.1.20", "onvif:http://10.0.0.5"]
    );
    // Without a turn cache every call discovers.
    let fresh = ReadOptions::new(Origin::Cli, &cancel);
    let args = json!({"discover": true, "include_simulated": true});
    run(&host, &ctx, "list_cameras", args, &fresh).unwrap();
    assert_eq!(host.ops(), ["discover", "discover"]);
    let args = json!({"discover": false, "include_simulated": false});
    let people = run(&host, &ctx, "list_cameras", args, &fresh)
        .unwrap()
        .value;
    assert_eq!(
        people["cameras"][1]["address"], FACETIME,
        "people see addresses"
    );
}

#[test]
fn adding_simulators_reuses_the_turns_first_discovery() {
    let ctx = fixture_context(fixture_snapshot());
    let host = FakeHost::new(fixture_snapshot());
    let cancel = AtomicBool::new(false);
    let turn = TurnCache::new();
    let mut opts = ReadOptions::new(Origin::Assistant, &cancel);
    opts.turn = Some(&turn);
    let first = run(
        &host,
        &ctx,
        "list_cameras",
        json!({"discover": true, "include_simulated": false}),
        &opts,
    )
    .unwrap();
    let later = run(
        &host,
        &ctx,
        "list_cameras",
        json!({"discover": true, "include_simulated": true}),
        &opts,
    )
    .unwrap();
    assert_eq!(host.ops(), ["discover"]);
    assert_eq!(first.value["discovery"]["reused"], false);
    assert_eq!(later.value["discovery"]["reused"], true);
    assert!(
        first.value["discovered"]
            .as_array()
            .unwrap()
            .iter()
            .all(|device| device["id"] != "sim:2")
    );
    assert!(
        later.value["discovered"]
            .as_array()
            .unwrap()
            .iter()
            .any(|device| device["id"] == "sim:2")
    );
}

#[test]
fn a_failed_discovery_is_not_retried_during_the_same_turn() {
    let ctx = fixture_context(fixture_snapshot());
    let host = FakeHost::new(fixture_snapshot())
        .reply("discover", Err("The discovery helper exited.".into()));
    let cancel = AtomicBool::new(false);
    let turn = TurnCache::new();
    let mut opts = ReadOptions::new(Origin::Assistant, &cancel);
    opts.turn = Some(&turn);
    let args = json!({"discover": true, "include_simulated": false});
    let first = run(&host, &ctx, "list_cameras", args.clone(), &opts).unwrap_err();
    let later = run(&host, &ctx, "list_cameras", args, &opts).unwrap_err();
    assert_eq!(first, later);
    assert_eq!(host.ops(), ["discover"]);
}

#[test]
fn destinations_and_storage_report_budget_and_locations() {
    let ctx = fixture_context(fixture_snapshot());
    let host = FakeHost::new(fixture_snapshot()).without_session();
    let cancel = AtomicBool::new(false);
    let opts = ReadOptions::new(Origin::Assistant, &cancel);
    let destinations = run(&host, &ctx, "list_destinations", json!({}), &opts)
        .unwrap()
        .value;
    assert_eq!(destinations["default"]["location"], "~/Capturefab");
    assert_eq!(
        destinations["destinations"][1]["location"],
        "s3://acme at https://minio.example:9000"
    );
    assert_eq!(destinations["destinations"][1]["remote"], true);
    let storage = run(&host, &ctx, "get_storage", json!({}), &opts)
        .unwrap()
        .value;
    assert_eq!(storage["used_bytes"], 1_800_000_000u64);
    assert_eq!(storage["free_bytes"], 10_737_418_240u64 - 1_800_000_000);
    assert_eq!(storage["free_files"], 9_880);
    assert_eq!(
        storage["summary"],
        "1.8 GB used and 0 B reserved of a 10.7 GB budget, so 8.9 GB is left; 7.1 GB free on disk."
    );
    assert_eq!(storage["when_full"], "New captures and recordings stop.");
    assert_eq!(storage["uploads"]["pending"], 1);
    assert_eq!(storage["uploads"]["last_error"], "upload to srt://h failed");
    let issues = run(&host, &ctx, "get_storage", json!({"verbose": true}), &opts).unwrap_err();
    assert_eq!(
        messages(&issues),
        [(
            Some("verbose"),
            "Remove `verbose`; get_storage has no such argument."
        )]
    );
}

#[test]
fn list_jobs_shows_schedules_and_recordings_per_camera() {
    let mut snapshot = fixture_snapshot();
    snapshot.cameras[0].jobs[0].skipped = 2;
    snapshot.cameras[0].jobs[0].retries = 1;
    snapshot.cameras[0].jobs[0].next_at_ms += 900_000;
    let ctx = fixture_context(snapshot);
    let host = FakeHost::new(fixture_snapshot());
    let cancel = AtomicBool::new(false);
    let opts = ReadOptions::new(Origin::Assistant, &cancel);
    let all = run(&host, &ctx, "list_jobs", json!({"camera": null}), &opts)
        .unwrap()
        .value;
    assert_clean(&all.to_string());
    let schedules = all["schedules"].as_array().unwrap();
    assert_eq!(schedules.len(), 2);
    assert_eq!(schedules[0]["id"], 7, "active jobs come first");
    assert_eq!(schedules[0]["every"], "5 min");
    assert_eq!(schedules[0]["next_at"], "2026-10-18T14:20:00+02:00");
    assert_eq!(schedules[0]["last_at"], "2026-10-19T13:10:00+02:00");
    assert_eq!(schedules[0]["skipped"], 2);
    assert_eq!(schedules[0]["retries"], 1);
    assert_eq!(schedules[0]["last_file"], "~/Capturefab/sim-0012.png");
    assert!(schedules[1].get("next_at").is_none());
    let recordings = all["recordings"].as_array().unwrap();
    assert_eq!(recordings.len(), 2);
    assert_eq!(recordings[1]["camera"], STREAM);
    assert_eq!(recordings[1]["until"], "when stopped");
    let issues = run(
        &host,
        &ctx,
        "list_jobs",
        json!({"camera": "Pattern camera"}),
        &opts,
    )
    .unwrap_err();
    assert!(issues[0].message.starts_with("More than one camera"));
    let one = run(&host, &ctx, "list_jobs", json!({"camera": "sim:1"}), &opts)
        .unwrap()
        .value;
    assert_eq!(one["schedules"], json!([]));
    assert_eq!(one["recordings"][0]["since"], "2026-10-18T13:50:00+02:00");
    let issues = run(&host, &ctx, "list_jobs", json!({}), &opts).unwrap_err();
    assert_eq!(
        messages(&issues),
        [(
            Some("camera"),
            "Provide `camera` (use null for the default)."
        )]
    );
}

#[test]
fn camera_features_list_curated_controls_and_hide_managed_ones() {
    let ctx = fixture_context(fixture_snapshot());
    let host = FakeHost::new(fixture_snapshot());
    let cancel = AtomicBool::new(false);
    let opts = ReadOptions::new(Origin::Assistant, &cancel);
    let features = |args: Value| {
        run(&host, &ctx, "get_camera_features", args, &opts)
            .unwrap()
            .value
    };
    let curated = features(json!({"camera": "sim:0", "search": null, "names": null}));
    assert_eq!(
        host.sent(),
        [(Some("sim:0".into()), json!({"op": "status"}))]
    );
    let names: Vec<_> = curated["features"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "ExposureTime",
            "Gain",
            "Width",
            "Height",
            "PixelFormat",
            "AcquisitionFrameRate"
        ]
    );
    let exposure = &curated["features"][0];
    assert_eq!(exposure["managed_by_auto"], true);
    assert_eq!(exposure["locked_while_streaming"], false);
    assert_eq!(exposure["unit"], "us");
    assert_eq!(exposure["max"], 1_000_000.0);
    assert!(exposure.get("choices").is_none() && exposure.get("inc").is_none());
    assert_eq!(curated["features"][2]["locked_while_streaming"], true);
    assert_eq!(curated["features"][4]["choices"], json!(["Mono8", "RGB8"]));
    assert_eq!(curated["streaming"], true);

    let named = features(json!({
        "camera": "sim:0",
        "search": null,
        "names": ["GevSCDA", "exposuretime", "Nope", "UserSetSave", "GevMissing"],
    }));
    assert_eq!(named["features"].as_array().unwrap().len(), 1);
    assert_eq!(named["missing"], json!(["Nope"]));
    assert_eq!(
        named["managed"],
        json!(["GevSCDA", "UserSetSave", "GevMissing"])
    );
    let search = features(json!({"camera": "sim:0", "search": "TEMP", "names": null}));
    assert_eq!(search["features"][0]["name"], "DeviceTemperature");
    assert_eq!(search["features"][0]["writable"], false);
    let everything = features(json!({"camera": "sim:0", "search": "", "names": null})).to_string();
    for hidden in [
        "GevSCDA",
        "TLParamsLocked",
        "UserSetSave",
        "LutValueReg",
        "TriggerSoftware",
    ] {
        assert!(!everything.contains(hidden), "{hidden}");
    }
    let webcam = features(json!({"camera": "FaceTime HD Camera", "search": null, "names": null}));
    assert!(
        webcam["note"]
            .as_str()
            .unwrap()
            .starts_with("Webcams have no exposure")
    );
    let issues = run(
        &host,
        &ctx,
        "get_camera_features",
        json!({"camera": "acA1920-40gm", "search": null, "names": null}),
        &opts,
    )
    .unwrap_err();
    assert_eq!(
        (issues[0].message.as_str(), issues[0].code),
        (
            "Connect the camera first; propose connect_camera.",
            IssueCode::Unavailable
        )
    );
}

#[test]
fn long_results_are_cut_to_the_output_bound_with_a_marker() {
    let mut snapshot = fixture_snapshot();
    snapshot.cameras[0].features = (0..800)
        .map(|n| FeatureInfo {
            description: "x".repeat(40),
            choices: vec!["Alpha".into(), "Beta".into()],
            ..feature(&format!("Custom{n:03}"), "Enumeration", json!("Alpha"))
        })
        .collect();
    let ctx = fixture_context(snapshot.clone());
    let host = FakeHost::new(snapshot);
    let cancel = AtomicBool::new(false);
    let opts = ReadOptions::new(Origin::Cli, &cancel);
    let value = run(
        &host,
        &ctx,
        "get_camera_features",
        json!({"camera": "sim:0", "search": "custom", "names": null}),
        &opts,
    )
    .unwrap()
    .value;
    let kept = value["features"].as_array().unwrap().len();
    assert!(size(&value) <= MAX_OUTPUT_BYTES, "{}", size(&value));
    assert!((100..800).contains(&kept), "{kept}");
    assert_eq!(
        value["truncated"],
        format!(
            "{} entries were left out to keep this result under 24 KiB; ask for less, for example one camera, a search or names.",
            grouped(800 - kept as u64)
        )
    );
}

fn frame_reply(image: Value) -> Value {
    json!({
        "camera": "sim:0",
        "frame": {"id": 812, "seq": 4031, "width": 640, "height": 480, "pixel_format": "Mono8",
                  "timestamp_ns": 1, "age_ms": 3, "source": "live", "waited_frames": 1},
        "region": {"x": 0, "y": 0, "width": 640, "height": 480, "normalized": [0.0, 0.0, 1.0, 1.0]},
        "exposure": {"mean": 0.43},
        "auto_meter": null,
        "focus": null,
        "notes": ["dark: mean brightness 0.12 is below 0.25"],
        "image": image,
    })
}

fn inspect_args(camera: &str) -> Value {
    json!({"camera": camera, "fresh": null, "max_dimension": null, "region": null, "include_image": null})
}

#[test]
fn inspect_frame_attaches_images_only_when_allowed() {
    use base64::Engine;
    let ctx = fixture_context(fixture_snapshot());
    let jpeg = b"\xff\xd8\xff\xe0 not really a jpeg".to_vec();
    let image = json!({
        "mime": "image/jpeg", "width": 320, "height": 240, "bytes": jpeg.len(),
        "base64": base64::engine::general_purpose::STANDARD.encode(&jpeg),
    });
    let host = FakeHost::new(fixture_snapshot()).reply("frame", Ok(frame_reply(image)));
    let cancel = AtomicBool::new(false);
    let mut opts = ReadOptions::new(Origin::Assistant, &cancel);
    opts.image_number = 3;
    let output = run(
        &host,
        &ctx,
        "inspect_frame",
        inspect_args("Pattern camera"),
        &opts,
    );
    assert!(
        output.is_err(),
        "an ambiguous name is refused before any frame"
    );
    let output = run(&host, &ctx, "inspect_frame", inspect_args("sim:0"), &opts).unwrap();
    assert_eq!(
        host.sent(),
        [(
            Some("sim:0".into()),
            json!({"op": "frame", "fresh": 1, "timeout_ms": 5_000, "max_dimension": 1_024, "format": "auto"})
        )]
    );
    assert_eq!(output.value["image"], "attached as Image 3");
    assert_eq!(output.value["name"], "Pattern camera");
    assert!(!output.value.to_string().contains("base64"));
    assert_eq!(output.images.len(), 1);
    let image = &output.images[0];
    assert_eq!(
        (image.mime, image.width, image.height),
        ("image/jpeg", 320, 240)
    );
    assert_eq!(&*image.data, &jpeg[..]);
    assert!(
        image
            .label
            .starts_with("Image 3: inspect_frame sim:0, frame 4031, ")
            && image.label.ends_with(", 320x240 of sensor [0,0,640,480]"),
        "{}",
        image.label
    );

    // Images off: the session is asked for measurements only.
    let host = FakeHost::new(fixture_snapshot()).reply("frame", Ok(frame_reply(Value::Null)));
    opts.allow_images = false;
    let args = json!({"camera": "sim:0", "fresh": 2, "max_dimension": 512,
                      "region": {"x": 0.25, "y": 0.25, "width": 0.5, "height": 0.5},
                      "include_image": true});
    let output = run(&host, &ctx, "inspect_frame", args, &opts).unwrap();
    assert_eq!(
        host.sent()[0].1,
        json!({"op": "frame", "fresh": 2, "timeout_ms": 5_000, "max_dimension": 0,
               "format": "auto", "region": [0.25, 0.25, 0.5, 0.5]})
    );
    assert_eq!(
        output.value["image"],
        "not sent: camera images are off, so only measurements are sent"
    );
    assert!(output.images.is_empty());
    assert_eq!(output.value["exposure"]["mean"], 0.43);

    let host = FakeHost::new(fixture_snapshot()).reply("frame", Ok(frame_reply(Value::Null)));
    opts.allow_images = true;
    let mut args = inspect_args("sim:0");
    args["include_image"] = json!(false);
    let output = run(&host, &ctx, "inspect_frame", args, &opts).unwrap();
    assert_eq!(output.value["image"], "not sent: include_image is false");
    assert!(matches!(
        host.calls()[0].1,
        SessionCommand::Frame {
            max_dimension: 0,
            ..
        }
    ));
}

#[test]
fn inspect_frame_checks_every_argument_and_reports_session_errors() {
    let ctx = fixture_context(fixture_snapshot());
    let host = FakeHost::new(fixture_snapshot()).reply(
        "frame",
        Err(
            "no new frame from sim:0 within 5 s (timed out); a triggered camera needs TriggerSoftware"
                .into(),
        ),
    );
    let cancel = AtomicBool::new(false);
    let opts = ReadOptions::new(Origin::Cli, &cancel);
    let args = json!({"camera": "sim:0", "fresh": 11, "max_dimension": 100,
                      "region": {"x": 0.5, "y": 0.0, "width": 0.6, "height": 1.0},
                      "include_image": null});
    let issues = run(&host, &ctx, "inspect_frame", args, &opts).unwrap_err();
    assert_eq!(
        messages(&issues),
        [
            (Some("fresh"), "Choose fresh from 0 to 10 frames."),
            (
                Some("max_dimension"),
                "Choose max_dimension 0 for measurements only, or 256 to 1,568 pixels."
            ),
            (
                Some("region"),
                "Keep region inside the frame: x, y, width and height from 0 to 1, width and height above 0, and x + width and y + height at most 1."
            ),
        ]
    );
    let issues = run(&host, &ctx, "inspect_frame", inspect_args(SCANNER), &opts).unwrap_err();
    assert!(
        issues[0]
            .message
            .starts_with("A document scanner cannot be inspected")
    );
    let issues = run(
        &host,
        &ctx,
        "inspect_frame",
        json!({"camera": "sim:0"}),
        &opts,
    )
    .unwrap_err();
    assert_eq!(issues.len(), 4, "every missing key is named: {issues:?}");
    assert!(host.calls().is_empty(), "nothing is sent for a bad call");
    let issues = run(&host, &ctx, "inspect_frame", inspect_args("sim:0"), &opts).unwrap_err();
    assert_eq!(
        (issues[0].message.as_str(), issues[0].code),
        (
            "No new frame from sim:0 within 5 s (timed out); a triggered camera needs TriggerSoftware.",
            IssueCode::Timeout
        )
    );
}

fn wait_args(camera: Option<&str>, until: &str, timeout: &str) -> Value {
    json!({"camera": camera, "until": until, "timeout": timeout, "frames": null, "job_id": null})
}

fn waited(host: &FakeHost, args: Value, opts: &ReadOptions) -> Value {
    let ctx = fixture_context(fixture_snapshot());
    run(host, &ctx, "wait_for", args, opts).unwrap().value
}

fn outcome(value: &Value) -> (bool, &str) {
    (
        value["satisfied"].as_bool().unwrap(),
        value["reason"].as_str().unwrap(),
    )
}

#[test]
fn wait_for_time_frames_and_auto_settling() {
    let cancel = AtomicBool::new(false);
    let opts = ReadOptions::new(Origin::Assistant, &cancel);
    let started = Instant::now();
    let host = FakeHost::new(fixture_snapshot());
    let time = waited(&host, wait_args(None, "time", "1s"), &opts);
    assert!(started.elapsed() >= Duration::from_secs(1));
    assert_eq!(outcome(&time), (true, "met"));
    assert!(time["waited_ms"].as_u64().unwrap() >= 1_000);
    assert!(host.calls().is_empty(), "waiting reads snapshots only");

    let mut later = fixture_snapshot();
    later.cameras[0].frames = 1003;
    let host = FakeHost::new(fixture_snapshot()).then(later);
    let mut args = wait_args(Some("sim:0"), "frames", "10s");
    args["frames"] = json!(2);
    let frames = waited(&host, args, &opts);
    assert_eq!(outcome(&frames), (true, "met"));
    assert_eq!(frames["state"]["new_frames"], 3);
    assert!(frames["waited_ms"].as_u64().unwrap() < 5_000);

    let mut settled = fixture_snapshot();
    settled.cameras[0].auto.as_mut().unwrap().state = "stable".into();
    let host = FakeHost::new(fixture_snapshot()).then(settled);
    let auto = waited(
        &host,
        wait_args(Some("sim:0"), "auto_settled", "10s"),
        &opts,
    );
    assert_eq!(outcome(&auto), (true, "met"));
    assert_eq!(auto["state"]["auto"]["state"], "stable");

    // A camera without auto exposure has nothing to settle.
    let host = FakeHost::new(fixture_snapshot());
    let off = waited(
        &host,
        wait_args(Some("sim:1"), "auto_settled", "10s"),
        &opts,
    );
    assert_eq!(outcome(&off), (true, "met"));
    assert!(
        off["note"]
            .as_str()
            .unwrap()
            .starts_with("Auto exposure is off")
    );
    assert!(off["waited_ms"].as_u64().unwrap() < 1_000);

    let host = FakeHost::new(fixture_snapshot());
    let timeout = waited(&host, wait_args(Some("sim:0"), "auto_settled", "1s"), &opts);
    assert_eq!(outcome(&timeout), (false, "timeout"));
}

#[test]
fn wait_for_jobs_recordings_and_other_participants() {
    let cancel = AtomicBool::new(false);
    let opts = ReadOptions::new(Origin::Assistant, &cancel);
    let mut done = fixture_snapshot();
    done.cameras[0].jobs[0].status = "complete".into();
    let host = FakeHost::new(fixture_snapshot()).then(done);
    let mut args = wait_args(None, "job_done", "10s");
    args["job_id"] = json!(7);
    let job = waited(&host, args, &opts);
    assert_eq!(outcome(&job), (true, "met"));
    assert_eq!(job["state"]["job"]["status"], "complete");

    let mut stopped = fixture_snapshot();
    stopped.cameras[3].forwarding = None;
    let host = FakeHost::new(fixture_snapshot()).then(stopped);
    let recording = waited(
        &host,
        wait_args(Some("sim:1"), "recording_stopped", "10s"),
        &opts,
    );
    assert_eq!(outcome(&recording), (true, "met"));
    assert_eq!(recording["state"]["recording"], Value::Null);

    let entry = |id: u64, actor: &str| -> SessionActivity {
        serde_json::from_value(json!({
            "id": id, "origin": "gui",
            "actor": {"id": actor, "name": "Desktop 12", "kind": "human"},
            "operation": "set", "camera": "sim:0", "feature": "ExposureTime",
            "label": "Set ExposureTime = 5000", "state": "succeeded",
            "submitted_at_ms": NOW, "started_at_ms": null, "finished_at_ms": null,
            "detail": null, "file_count": 0, "files": [],
        }))
        .unwrap()
    };
    let mut ours = fixture_snapshot();
    ours.activity = vec![entry(1, "assistant:1:ab12")];
    let mut theirs = ours.clone();
    theirs.activity.push(entry(2, "desktop:12"));
    let host = FakeHost::new(fixture_snapshot())
        .then(ours)
        .then(theirs)
        .with_actor("assistant:1:ab12");
    let activity = waited(&host, wait_args(Some("sim:0"), "activity", "10s"), &opts);
    assert_eq!(outcome(&activity), (true, "met"));
    assert_eq!(
        activity["note"],
        "Desktop 12: Set ExposureTime = 5000 on Pattern camera"
    );
    assert!(
        activity["waited_ms"].as_u64().unwrap() >= 400,
        "our own change does not end the wait"
    );
}

#[test]
fn wait_for_stops_on_cancel_and_human_changes() {
    let cancel = AtomicBool::new(true);
    let opts = ReadOptions::new(Origin::Assistant, &cancel);
    let host = FakeHost::new(fixture_snapshot());
    let stopped = waited(&host, wait_args(None, "time", "10s"), &opts);
    assert_eq!(outcome(&stopped), (false, "stopped"));
    assert!(stopped["waited_ms"].as_u64().unwrap() < 1_000);

    let cancel = AtomicBool::new(false);
    let asked = AtomicUsize::new(0);
    let interrupt =
        || (asked.fetch_add(1, Ordering::SeqCst) >= 2).then(|| "Ann set Gain on sim:0".to_owned());
    let mut opts = ReadOptions::new(Origin::Assistant, &cancel);
    opts.interrupt = Some(&interrupt);
    let host = FakeHost::new(fixture_snapshot());
    let changed = waited(&host, wait_args(Some("sim:0"), "time", "10s"), &opts);
    assert_eq!(outcome(&changed), (false, "human_change"));
    assert_eq!(changed["note"], "Ann set Gain on sim:0");
    assert_eq!(asked.load(Ordering::SeqCst), 3, "asked once per slice");
    assert_eq!(changed["state"]["camera"], "sim:0");
}

#[test]
fn wait_for_checks_its_arguments_first() {
    let cancel = AtomicBool::new(false);
    let mut opts = ReadOptions::new(Origin::Assistant, &cancel);
    let ctx = fixture_context(fixture_snapshot());
    let host = FakeHost::new(fixture_snapshot());
    let check = |args: Value, opts: &ReadOptions| -> Vec<(Option<String>, String)> {
        run(&host, &ctx, "wait_for", args, opts)
            .unwrap_err()
            .into_iter()
            .map(|issue| (issue.field, issue.message))
            .collect()
    };
    let one = |field: &str, message: &str| vec![(Some(field.to_owned()), message.to_owned())];
    assert_eq!(
        check(wait_args(None, "time", "90"), &opts),
        one("timeout", "Add a unit, for example 90s, 15m or 1h30m.")
    );
    assert_eq!(
        check(wait_args(None, "time", "11m"), &opts),
        one("timeout", "Choose a timeout of at most 10 min.")
    );
    assert_eq!(
        check(wait_args(None, "time", "500ms"), &opts),
        one("timeout", "Choose a timeout of at least 1 s.")
    );
    assert_eq!(
        check(wait_args(None, "frames", "10s"), &opts),
        vec![
            (
                Some("camera".into()),
                "Choose the camera to watch; this condition needs one.".into()
            ),
            (
                Some("frames".into()),
                "Choose frames from 1 to 1,000 new frames.".into()
            ),
        ]
    );
    let mut args = wait_args(None, "time", "10s");
    args["job_id"] = json!(7);
    assert_eq!(
        check(args, &opts),
        one(
            "job_id",
            "Set job_id only when until is job_done; use null."
        )
    );
    let mut args = wait_args(None, "job_done", "10s");
    args["job_id"] = json!(99);
    let issues = run(&host, &ctx, "wait_for", args, &opts).unwrap_err();
    assert_eq!(
        (issues[0].message.as_str(), issues[0].code),
        (
            "There is no job 99; list_jobs shows job IDs.",
            IssueCode::Unavailable
        )
    );
    let mut args = wait_args(Some("FaceTime HD Camera"), "frames", "10s");
    args["frames"] = json!(5);
    assert!(
        check(args, &opts)[0]
            .1
            .starts_with("This camera is not streaming")
    );
    opts.max_wait = Duration::from_secs(30);
    assert_eq!(
        check(wait_args(None, "time", "1m"), &opts),
        one(
            "timeout",
            "Choose a timeout of at most 30 s, the time this task has left."
        )
    );
    assert_eq!(
        check(wait_args(None, "soon", "1m"), &opts),
        one(
            "until",
            "`until` must be one of time, frames, auto_settled, job_done, recording_stopped or activity."
        )
    );
}

#[test]
fn only_observation_tools_run_here() {
    let ctx = fixture_context(fixture_snapshot());
    let host = FakeHost::new(fixture_snapshot());
    let cancel = AtomicBool::new(false);
    let opts = ReadOptions::new(Origin::Assistant, &cancel);
    let issues = run(&host, &ctx, "rtsp://u:p@h/s", json!({}), &opts).unwrap_err();
    assert_eq!(
        issues[0].message,
        "rtsp://h is not an observation tool; list the tools for their names."
    );
    let issues = run(&host, &ctx, "get_overview", json!([]), &opts).unwrap_err();
    assert_eq!(issues[0].field, None);
}

#[test]
fn durations_take_compound_units_largest_first() {
    for (text, ms) in [
        ("90s", 90_000),
        ("1h30m", 5_400_000),
        ("1 hour", 3_600_000),
        ("1h 30 min", 5_400_000),
        ("2.5h", 9_000_000),
        ("1D", 86_400_000),
        ("1m0s", 60_000),
        ("1s500ms", 1_500),
        ("10 minutes", 600_000),
    ] {
        assert_eq!(parse_duration_field(text), Ok(ms), "{text}");
    }
    for (text, message) in [
        ("2", "Add a unit, for example 90s, 15m or 1h30m."),
        (
            "30m1h",
            "Write each unit once, from largest to smallest, for example 1h30m.",
        ),
        (
            "1m1m",
            "Write each unit once, from largest to smallest, for example 1h30m.",
        ),
        (
            "5 fortnights",
            "Use the units ms, s, m, h or d, for example 90s, 15m or 1h30m.",
        ),
        (
            "h",
            "Write the duration as a number and a unit, for example 90s, 15m or 1h30m.",
        ),
        ("0s", "Choose a duration longer than zero."),
        ("1.0005s", "Use whole milliseconds, for example 1.5s."),
        ("3700d", "Choose a duration of at most 10 years."),
        ("", "Enter a duration, for example 90s, 15m or 1h30m."),
    ] {
        assert_eq!(
            parse_duration_field(text),
            Err(message.to_owned()),
            "{text}"
        );
    }
    for ms in [1, 1_500, 90_000, 5_400_000, 86_400_000 + 1] {
        let text = format_duration_field(ms);
        assert_eq!(parse_duration_field(&text), Ok(ms), "{text}");
    }
    assert_eq!(format_duration_field(5_400_000), "1h30m");
    assert_eq!(format_duration_field(90_000), "1m30s");
}

#[test]
fn issue_codes_follow_the_cli_exit_codes() {
    for (message, code) in [
        (
            "no new frame from sim:0 within 3 s (timed out)",
            IssueCode::Timeout,
        ),
        ("camera 'sim:4' not found", IssueCode::Unavailable),
        (
            "JPEG previews are unsupported in this build",
            IssueCode::Unsupported,
        ),
        ("Storage full: budget reached", IssueCode::StorageFull),
        ("the encoder failed", IssueCode::OperationFailed),
    ] {
        assert_eq!(
            IssueCode::of_error(&anyhow::anyhow!(message)),
            code,
            "{message}"
        );
    }
    assert_eq!(IssueCode::Timeout.exit(), ("timeout", 4));
    assert_eq!(IssueCode::default(), IssueCode::Usage);
    let issue = Issue::failed(
        &anyhow::anyhow!("camera 'rtsp://u:p@h/s' not found in this session"),
        &json!(["rtsp://u:p@h/s"]),
        Origin::Cli,
    );
    assert_eq!(
        issue.message,
        "Camera 'rtsp://<redacted>@h/s' not found in this session."
    );
    assert_eq!(
        serde_json::to_value(&issue).unwrap(),
        json!({"field": null, "message": issue.message, "severity": "error", "code": "unavailable"})
    );
    assert_eq!(sentence("done!"), "Done!");
    assert_eq!(list(&["a", "b", "c"]), "a, b and c");
    assert_eq!(grouped(1_568), "1,568");
    assert_eq!(grouped(100), "100");
    assert_eq!(grouped(1_000_000), "1,000,000");
    assert_eq!(format_bytes(512), "512 B");
    assert_eq!(format_bytes(1_800_000_000), "1.8 GB");
}

/// `Context::gather`'s inputs, fixed.
struct FakeSources {
    destinations: Vec<Destination>,
    missing_volume: Option<PathBuf>,
}

impl Sources for FakeSources {
    fn destinations(&self) -> Result<Vec<Destination>> {
        Ok(self.destinations.clone())
    }

    fn configured_policy(&self) -> Result<StoragePolicy> {
        Ok(StoragePolicy {
            max_bytes: 1 << 30,
            on_full: "delete-oldest".into(),
            ..StoragePolicy::default()
        })
    }

    fn quota(&self) -> Result<Value> {
        Ok(json!({
            "ledger": home().join(".cache/capturefab/sessions/storage.json"),
            "allocated_bytes": 5_000,
            "reserved_bytes": 600,
            "files": 2,
        }))
    }

    fn encoders(&self) -> Result<Arc<Vec<String>>> {
        Ok(Arc::new(vec![
            "libx265".into(),
            "libsvtav1".into(),
            "png".into(),
        ]))
    }

    fn uploads(&self) -> Option<Result<Value>> {
        Some(Err(anyhow::anyhow!("uploads.json is locked")))
    }

    fn free_space(&self, path: &Path) -> Option<u64> {
        (!path.starts_with("/Volumes/Gone")).then_some(42)
    }

    fn volume_problem(&self, path: &Path) -> Option<String> {
        if self.missing_volume.as_deref() == Some(path) {
            return Some("The staging volume is disconnected.".into());
        }
        path.starts_with("/Volumes/Gone").then(|| {
            "external volume /Volumes/Gone is not mounted; reconnect it or choose another destination"
                .into()
        })
    }
}

#[test]
fn gathering_reads_destinations_storage_and_encoders_without_secrets() {
    let bucket = Bucket {
        endpoint: "https://minio.example:9000".into(),
        region: "us-east-1".into(),
        bucket: "acme".into(),
        prefix: "cams/".into(),
        path_style: true,
        credentials: Credentials::Keychain {
            access_key_id: "AKIAIOSFODNN7EXAMPLE".into(),
        },
        keep_local: false,
        no_overwrite: Default::default(),
    };
    let sources = FakeSources {
        missing_volume: None,
        destinations: vec![
            Destination {
                name: "line-1".into(),
                target: Target::Folder {
                    path: home().join("Captures/line-1"),
                },
            },
            Destination {
                name: "usb".into(),
                target: Target::Folder {
                    path: "/Volumes/Gone/captures".into(),
                },
            },
            Destination {
                name: "archive".into(),
                target: Target::S3(bucket),
            },
        ],
    };
    let host = FakeHost::new(fixture_snapshot());
    let ctx = gather_with(
        &sources,
        &host,
        home().join("Capturefab"),
        StoragePolicy::default(),
        RecordingDefaults::default(),
    )
    .unwrap();
    assert!(ctx.session && ctx.snapshot.cameras.len() == 5);
    assert_eq!(ctx.destinations[0].location, "~/Captures/line-1");
    assert_eq!(ctx.destinations[0].free_bytes, Some(42));
    assert_eq!(
        ctx.destinations[1].problem.as_deref(),
        Some(
            "external volume /Volumes/Gone is not mounted; reconnect it or choose another destination"
        )
    );
    let archive = &ctx.destinations[2];
    assert_eq!(
        archive.location,
        "s3://acme/cams/ at https://minio.example:9000"
    );
    assert_eq!(archive.host.as_deref(), Some("minio.example:9000"));
    assert!(archive.remote && !archive.insecure);
    assert_eq!(archive.problem.is_some(), !cfg!(feature = "s3"));
    // The request policy is clamped by the configured one.
    assert_eq!(ctx.storage.effective.max_bytes, 1 << 30);
    assert_eq!(ctx.storage.effective.on_full, "delete-oldest");
    assert_eq!(
        ctx.storage.request.max_bytes,
        StoragePolicy::default().max_bytes
    );
    assert_eq!(ctx.storage.free_budget(), (1 << 30) - 5_000);
    assert_eq!(ctx.media.encoders_by_codec["hevc"], ["libx265"]);
    assert_eq!(ctx.media.encoders_by_codec["av1"], ["libsvtav1"]);
    assert!(ctx.media.encoders_by_codec["h264"].is_empty() && ctx.media.available);
    assert_eq!(
        ctx.uploads.as_ref().unwrap().problem.as_deref(),
        Some("uploads.json is locked")
    );
    let text = format!("{ctx:?}");
    assert!(
        !text.contains("AKIA") && !text.contains("storage.json"),
        "{text}"
    );

    let none = gather_with(
        &sources,
        &NoSession,
        PathBuf::from("Capturefab"),
        StoragePolicy::default(),
        RecordingDefaults::default(),
    )
    .unwrap();
    assert!(!none.session && none.snapshot.cameras.is_empty());
}

#[test]
fn destinations_of_kinds_this_build_lacks_carry_a_problem() {
    let compiled = Compiled {
        s3: false,
        ftp: false,
        sftp: false,
        uploads: false,
        jpeg: false,
    };
    let sources = FakeSources {
        destinations: Vec::new(),
        missing_volume: None,
    };
    let s3 = Destination {
        name: "archive".into(),
        target: Target::S3(Bucket {
            endpoint: "http://nas.local:9000".into(),
            region: "us-east-1".into(),
            bucket: "acme".into(),
            prefix: String::new(),
            path_style: true,
            credentials: Credentials::Environment,
            keep_local: false,
            no_overwrite: Default::default(),
        }),
    };
    let info = DestinationInfo::new(&s3, &compiled, &sources);
    assert_eq!(
        info.problem.as_deref(),
        Some("S3 destinations need a build with --features s3")
    );
    assert!(info.insecure);
    assert!(!compiled.supports("ftp") && compiled.supports("folder"));

    let compiled = Compiled {
        s3: true,
        ..compiled
    };
    let sources = FakeSources {
        destinations: Vec::new(),
        missing_volume: Some(s3.local_root()),
    };
    let info = DestinationInfo::new(&s3, &compiled, &sources);
    assert_eq!(
        info.problem.as_deref(),
        Some("The staging volume is disconnected.")
    );
}

#[test]
fn hosts_read_snapshots_and_send_commands() {
    let snapshot = fixture_snapshot();
    let status = serde_json::to_value(&snapshot).unwrap();
    let parsed = snapshot_from(status).unwrap();
    assert_eq!(parsed.cameras.len(), 5);
    assert_eq!(parsed.cameras[3].forwarding_until_ms, Some(NOW + 1_800_000));
    assert!(snapshot_from(json!({"cameras": 3})).is_err());

    assert!(!NoSession.has_session());
    assert!(NoSession.snapshot().unwrap().cameras.is_empty());
    let refused = NoSession.execute(None, SessionCommand::Status).unwrap_err();
    assert!(
        refused
            .to_string()
            .starts_with("this command needs a running session")
    );

    let handle = SessionHandle::new();
    let actor = SessionActor::new("assistant:1:ab12", "Assistant", ActorKind::Agent).unwrap();
    let host = SessionHost::new(handle.clone(), ActivityOrigin::Gui, actor);
    assert!(host.has_session());
    assert_eq!(host.actor_id().as_deref(), Some("assistant:1:ab12"));
    assert!(host.snapshot().unwrap().cameras.is_empty());
    let status = host.execute(None, SessionCommand::Status).unwrap();
    assert!(status["cameras"].as_array().unwrap().is_empty());
    let mut ctx = fixture_context(host.snapshot().unwrap());
    ctx.destinations.clear();
    let cancel = AtomicBool::new(false);
    let opts = ReadOptions::new(Origin::Assistant, &cancel);
    let overview = run(&host, &ctx, "get_overview", json!({}), &opts)
        .unwrap()
        .value;
    assert_eq!(overview["cameras"], json!([]));
    handle.shutdown();
}
