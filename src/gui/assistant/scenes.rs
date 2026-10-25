//! Display-only quick command fixtures. Neither pass gathers context, resolves
//! credentials, starts a worker or changes the real session.
use super::*;
use crate::{
    assistant::{agent::Plan, conversation::ImagePart, provider::ProviderId},
    tools::{
        ToolCall,
        context::{
            Compiled, Context, DestinationInfo, MediaInfo, RecordingDefaults, StorageInfo, Zone,
        },
    },
};
use serde_json::json;
use std::sync::Arc;

pub(in crate::gui) const WORDS: [&str; 11] = [
    "quick-command",
    "quick-command-data-consent",
    "quick-command-plan",
    "quick-command-setup-restore",
    "quick-command-invalid",
    "quick-command-running",
    "quick-command-done",
    "quick-command-setup",
    "quick-command-thinking",
    "quick-command-error",
    "quick-command-exported",
];

impl Workbench {
    pub(in crate::gui) fn scene_assistant_quick(&mut self, word: &str, _late: bool) -> bool {
        if !WORDS.contains(&word) {
            return false;
        }
        self.assistant.quick = fixture(word, self.snapshot.clone());
        true
    }
}

pub(in crate::gui) fn fixture(word: &str, snapshot: SessionSnapshot) -> quick::State {
    let mut state = quick::State::with_services(Arc::new(super::runner::Unavailable));
    state.synthetic = true;
    state.open = true;
    state.settings.provider = Some(ProviderId::Anthropic);
    state.settings.providers.insert(
        ProviderId::Anthropic,
        crate::assistant::provider::ProviderSettings {
            saved_key: true,
            ..Default::default()
        },
    );
    state.context = Some(context(snapshot));
    if word == "quick-command-setup" {
        state.settings.provider = None;
        return state;
    }
    if word == "quick-command-data-consent" {
        state.allow_data = true;
        state.allow_images = true;
        return state;
    }
    if word == "quick-command-exported" {
        let context = state.context.as_mut().unwrap();
        #[cfg(windows)]
        {
            context.capture_root = PathBuf::from(r"C:\Capturefab Review\captures");
        }
        state
            .plan
            .propose(
                ToolCall {
                    tool: "get_overview".into(),
                    args: json!({}),
                },
                None,
            )
            .unwrap();
        state
            .plan
            .recheck(context, None, &crate::tools::EmptySecrets);
        state.exported_plan = Some(context.capture_root.join("reviewed-plan.json"));
        return state;
    }
    if word == "quick-command" {
        return state;
    }
    if word == "quick-command-setup-restore" {
        setup_restore(&mut state);
        return state;
    }
    state.transcript.push(quick::TranscriptEntry::User(
        "Record the camera for 30 seconds and take one photo for the run log.".into(),
    ));
    state.allow_images = true;
    if word == "quick-command-thinking" {
        state.scene_thinking = true;
        state.status = "Asking Anthropic…".into();
        return state;
    }
    if word == "quick-command-error" {
        state.error = Some("The AI provider could not complete this request. Try again.".into());
        state.last_prompt =
            Some("Record the camera for 30 seconds and take one photo for the run log.".into());
        state.provider_excerpt = Some("The service is temporarily busy.".into());
        return state;
    }
    state.transcript.push(quick::TranscriptEntry::Tool {
        title: "Inspected the camera".into(),
        summary: "Mean brightness 0.46 · highlights 0.1 % · the exposure looks balanced.".into(),
        images: vec![sheet::SentImage::new(image())],
    });
    state.transcript.push(quick::TranscriptEntry::Assistant(
        "The exposure looks balanced. This plan saves a short recording and one PNG photo in your capture folder.".into(),
    ));
    if word == "quick-command-done" {
        state.transcript.push(quick::TranscriptEntry::Outcome {
            summary: "Recording started. It stops after 30 seconds.".into(),
            level: Level::Done,
        });
        state.transcript.push(quick::TranscriptEntry::Outcome {
            summary: "Saved camera-0001.png in the capture folder.".into(),
            level: Level::Done,
        });
        return state;
    }
    let camera = state.context.as_ref().unwrap().snapshot.cameras[0]
        .info
        .id
        .clone();
    let mut plan = Plan::default();
    let first = plan
        .propose(
            ToolCall {
                tool: "start_recording".into(),
                args: json!({"camera":camera,"duration":"30s","file_name":"run-{datetime}.mp4","bitrate":null,"codec":null,"destination":null,"fps":null,"tags":null}),
            },
            None,
        )
        .unwrap();
    let photo = ToolCall {
        tool: "capture_photos".into(),
        args: json!({"camera":camera,"count":1,"format":"png","file_name":"run-{seq}.png","destination":null,"tags":null}),
    };
    let second = plan.propose(photo.clone(), None).unwrap();
    if word == "quick-command-invalid" {
        plan.edit(second, photo.clone(), true).unwrap();
        let mut changed = photo;
        changed.args["camera"] = json!("gige:camera-unavailable");
        changed.args["count"] = json!("ten");
        plan.propose(changed, Some(second)).unwrap();
    }
    plan.recheck(
        state.context.as_ref().unwrap(),
        None,
        &crate::tools::EmptySecrets,
    );
    state.plan = plan;
    if word == "quick-command-running" {
        state.scene_running = true;
        state.status = "Running step 1 of 2…".into();
        state.step_outcomes.insert(
            first,
            quick::StepStatus {
                state: quick::StepPhase::Running,
                message: "Starting the recording…".into(),
            },
        );
    }
    state
}

/// A restore of a saved conveyor setup that changes trigger and line
/// settings: a human-only step with more facts than the card lists, banked
/// labels and the reasons a person must approve it.
fn setup_restore(state: &mut quick::State) {
    state.transcript.push(quick::TranscriptEntry::User(
        "Put the camera back on the conveyor-line setup.".into(),
    ));
    let context = state.context.as_mut().unwrap();
    let camera = &mut context.snapshot.cameras[0];
    let feature = |name: &str, kind: &str, value: serde_json::Value| crate::genicam::FeatureInfo {
        name: name.into(),
        display_name: name.into(),
        kind: kind.into(),
        value: Some(value),
        writable: true,
        description: String::new(),
        unit: None,
        min: None,
        max: None,
        inc: None,
        choices: Vec::new(),
        error: None,
    };
    camera.features = vec![
        feature("ExposureTime", "Float", json!(12000.0)),
        feature("Gain", "Float", json!(0.0)),
        feature("TriggerSelector", "Enumeration", json!("FrameStart")),
        feature("TriggerMode", "Enumeration", json!("Off")),
        feature("TriggerSource", "Enumeration", json!("Software")),
        feature("TriggerActivation", "Enumeration", json!("RisingEdge")),
        feature("LineSelector", "Enumeration", json!("Line0")),
        feature("LineMode", "Enumeration", json!("Input")),
        feature("LineInverter", "Boolean", json!(false)),
    ]
    .into();
    let id = camera.info.id.clone();
    let mut plan = Plan::default();
    plan.propose(
        ToolCall {
            tool: "apply_camera_setup".into(),
            args: json!({"camera":id,"name":"conveyor-line","allow_other_model":false}),
        },
        None,
    )
    .unwrap();
    plan.recheck(context, None, &crate::tools::EmptySecrets);
    state.plan = plan;
}

/// The setups the scene context lists: a conveyor setup saved from the
/// scene's camera, with trigger and line values at several banks.
fn fixture_setups(info: &crate::types::CameraInfo) -> Vec<crate::tools::context::SavedSetup> {
    use crate::setup::{Bank, SavedCamera, Scalar, Setup, SetupValue};
    let value = |feature: &str, value: Scalar, at: &[(&str, &str)]| SetupValue {
        feature: feature.into(),
        value,
        at: at
            .iter()
            .map(|(selector, value)| Bank {
                selector: (*selector).into(),
                value: (*value).into(),
            })
            .collect(),
    };
    let text = |value: &str| Scalar::String(value.into());
    let number = |value: f64| Scalar::Number(serde_json::Number::from_f64(value).unwrap());
    let mut values = vec![
        value("ExposureTime", number(4000.0), &[]),
        value("Gain", number(6.0), &[]),
    ];
    for bank in ["FrameStart", "LineStart", "ExposureActive"] {
        let at = [("TriggerSelector", bank)];
        values.push(value("TriggerMode", text("On"), &at));
        values.push(value("TriggerSource", text("Line1"), &at));
        values.push(value("TriggerActivation", text("FallingEdge"), &at));
    }
    for line in ["Line0", "Line1", "Line2"] {
        let at = [("LineSelector", line)];
        values.push(value("LineMode", text("Output"), &at));
        values.push(value("LineInverter", Scalar::Bool(true), &at));
    }
    values.push(value("TriggerSelector", text("FrameStart"), &[]));
    values.push(value("LineSelector", text("Line0"), &[]));
    vec![crate::tools::context::SavedSetup::new(Setup {
        version: 1,
        name: "conveyor-line".into(),
        saved_at: "2026-10-18T10:00:00Z".into(),
        note: Some("Encoder-triggered line, strobe on Line0".into()),
        camera: SavedCamera::from_info(info),
        auto: None,
        values,
        skipped: Vec::new(),
    })]
}

pub(in crate::gui) fn context(mut snapshot: SessionSnapshot) -> Context {
    if snapshot.cameras.is_empty() {
        let mut info = crate::transport::simulator::info();
        info.model = "Workbench camera".into();
        snapshot.devices = vec![info.clone()];
        snapshot.connected = Some(info.clone());
        snapshot.active_camera = Some(info.id.clone());
        snapshot.cameras.push(
            serde_json::from_value(json!({
                "info":info,"features":[],"streaming":true,"frames":240,
                "dropped":0,"fps":30.0,"last_error":null,"worker_pid":12345
            }))
            .unwrap(),
        );
    }
    let policy = StoragePolicy::default();
    let setups = snapshot
        .cameras
        .first()
        .map(|camera| fixture_setups(&camera.info))
        .unwrap_or_default();
    Context {
        now_ms: 1_792_344_960_000,
        zone: Zone::Fixed(-14_400),
        session: true,
        snapshot,
        allow_data: false,
        data_dataset: None,
        destinations: vec![DestinationInfo {
            approval_identity: None,
            name: "inspection-archive".into(),
            kind: "folder".into(),
            location: "~/Capturefab/Inspection".into(),
            host: None,
            insecure: false,
            problem: None,
            free_bytes: Some(40_000_000_000),
            remote: false,
        }],
        storage: StorageInfo {
            request: policy.clone(),
            effective: policy,
            allocated_bytes: 1_200_000_000,
            reserved_bytes: 0,
            files: 120,
            free_disk: Some(40_000_000_000),
        },
        media: MediaInfo::from_names(&["libx264".into(), "libx265".into()]),
        capture_root: PathBuf::from(if cfg!(windows) {
            r"C:\tmp\capturefab-scene\captures"
        } else {
            "/tmp/capturefab-scene/captures"
        }),
        compiled: Compiled::current(),
        defaults: RecordingDefaults::default(),
        setups,
        setups_problem: None,
    }
}

/// A bounded PNG fixture. The thumbnail is decoded from this same byte array.
pub(in crate::gui) fn image() -> ImagePart {
    let (width, height) = (96, 64);
    let mut pixels = Vec::with_capacity((width * height * 3) as usize);
    for y in 0..height {
        for x in 0..width {
            let center = (28..68).contains(&x) && (14..50).contains(&y);
            pixels.extend_from_slice(if center {
                &[205, 93, 54]
            } else {
                &[50, 78, 94]
            });
        }
    }
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, width, height);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .write_header()
            .unwrap()
            .write_image_data(&pixels)
            .unwrap();
    }
    ImagePart {
        mime: "image/png",
        data: Arc::from(bytes),
        width,
        height,
        label: "Image 1: inspected workbench camera frame".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quick_command_scenes_never_start_workers_or_change_the_session() {
        let mut app = Workbench::new(
            SessionHandle::new(),
            "quick-command-scenes".into(),
            true,
            None,
        );
        for word in WORDS {
            for late in [false, true] {
                assert!(app.scene_assistant_quick(word, late));
                assert!(app.assistant.quick.open);
                assert!(app.assistant.quick.synthetic);
                assert!(app.handle.snapshot().cameras.is_empty());
                assert!(app.handle.snapshot().activity.is_empty());
            }
        }
        assert!(!app.scene_assistant_quick("unrelated", false));
    }

    #[test]
    fn scene_plans_are_checked_and_the_invalid_scene_keeps_the_bad_camera_visible() {
        let valid = fixture("quick-command-plan", SessionSnapshot::default());
        assert_eq!(valid.plan.steps.len(), 2);
        assert!(valid.ready_to_run(), "{:?}", valid.plan);
        let invalid = fixture("quick-command-invalid", SessionSnapshot::default());
        assert!(!invalid.ready_to_run());
        assert!(invalid.plan.steps[1].changed_by_assistant);
        assert!(invalid.blocking_step().is_some());
        assert!(fixture("quick-command-running", SessionSnapshot::default()).running());
        assert!(fixture("quick-command-thinking", SessionSnapshot::default()).thinking());
    }
}
