use super::*;
use crate::{
    assistant::run::RunEvent,
    tools::tests::{fixture_context, fixture_snapshot},
};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::{
    Mutex,
    atomic::{AtomicBool, AtomicUsize},
};

#[derive(Default)]
struct Stub {
    calls: AtomicUsize,
    hold: AtomicBool,
    requests: Mutex<Vec<run::RunRequest>>,
}
impl worker::Services for Stub {
    fn gather(&self) -> anyhow::Result<Context> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(fixture_context(fixture_snapshot()))
    }
    fn run(
        &self,
        _: Settings,
        request: run::RunRequest,
        control: &run::RunControl,
        _approver: Arc<worker::GuiApprover>,
        events: &mut dyn FnMut(RunEvent),
    ) -> anyhow::Result<run::RunOutcome> {
        self.requests.lock().unwrap().push(request);
        self.calls.fetch_add(1, Ordering::SeqCst);
        events(RunEvent::State(run::RunSnapshot {
            run_id: "r-20261018-123456-abcd".into(),
            actor_id: "assistant:test:task".into(),
            state: RunState::Running,
            ..Default::default()
        }));
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.hold.load(Ordering::SeqCst) && !control.cancel().load(Ordering::SeqCst) {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        let outcome = run::RunOutcome {
            run_id: "r-20261018-123456-abcd".into(),
            outcome: if control.cancel().load(Ordering::SeqCst) {
                run::Outcome::Stopped
            } else {
                run::Outcome::Finished
            },
            summary: "A synthetic task ended.".into(),
            totals: Default::default(),
            leftovers: Vec::new(),
            log: PathBuf::from("/tmp/synthetic-task.jsonl"),
            restore: Default::default(),
        };
        events(RunEvent::Finished(Box::new(outcome.clone())));
        Ok(outcome)
    }
    fn apply(
        &self,
        _: &[(StepId, ToolCall)],
        _: &[Checked],
        _: &worker::RecoveryGuard,
        _: &std::sync::atomic::AtomicBool,
    ) -> Result<Value, tools::ApplyError> {
        panic!("unapproved effect")
    }
    fn runs(&self) -> anyhow::Result<Vec<ai::log::RunSummary>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(Vec::new())
    }
    fn read_log(&self, _: &str) -> anyhow::Result<Vec<Value>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(vec![json!({"kind":"end","summary":"Synthetic"})])
    }
}
fn configured(stub: Arc<Stub>) -> State {
    let mut state = State::with_services(stub.clone());
    let settings = Settings {
        provider: Some(ai::ProviderId::Anthropic),
        ..Settings::default()
    };
    state.configure(settings, stub);
    state.mode = Mode::Supervised;
    state.goal = "Verify the camera exposure.".into();
    state.set_context(fixture_context(fixture_snapshot()), Some("sim:0".into()));
    state
}
fn drive(state: &mut State, mut ready: impl FnMut(&State) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        state.tick(Instant::now());
        if ready(state) {
            break;
        }
        assert!(Instant::now() < deadline, "synthetic task did not respond");
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn scope_defaults_once_and_an_explicit_empty_choice_stays_empty() {
    let mut state = configured(Arc::new(Stub::default()));
    assert!(state.cameras.contains("sim:0"));
    state.input(Input::Camera("sim:0".into(), false), Instant::now());
    state.set_context(fixture_context(fixture_snapshot()), Some("sim:1".into()));
    assert!(state.cameras.is_empty());
    assert!(!state.valid_to_start());
}

#[test]
fn dataset_choice_is_pinned_at_selection_and_does_not_follow_reopen() {
    let mut state = configured(Arc::new(Stub::default()));
    state.cameras.clear();
    state.context.as_mut().unwrap().snapshot.dataset = Some(crate::dataset::DatasetStatus {
        slot_id: Some("a".repeat(32)),
        name: "Line 1".into(),
        ..Default::default()
    });
    state.input(Input::UseDataset(true), Instant::now());
    state.input(Input::AllowData(true), Instant::now());
    assert!(state.valid_to_start(), "{:?}", state.start_reason());
    state
        .context
        .as_mut()
        .unwrap()
        .snapshot
        .dataset
        .as_mut()
        .unwrap()
        .slot_id = Some("b".repeat(32));
    assert!(state.start_reason().unwrap().contains("closed or replaced"));
    assert_eq!(state.dataset, Some("a".repeat(32)));
    state.input(Input::UseDataset(true), Instant::now());
    assert_eq!(state.dataset, Some("b".repeat(32)));
    assert!(state.valid_to_start());
}

#[test]
fn a_paused_task_cannot_resume_after_its_dataset_is_replaced() {
    let mut state = configured(Arc::new(Stub::default()));
    state.allow_data = true;
    state.dataset = Some("a".repeat(32));
    state.context.as_mut().unwrap().snapshot.dataset = Some(crate::dataset::DatasetStatus {
        slot_id: Some("b".repeat(32)),
        ..Default::default()
    });
    state.scene_active = true;
    state.snapshot.state = RunState::Paused;
    assert!(!state.can_resume());
    assert!(
        state
            .data_scope_issue()
            .unwrap()
            .contains("start a new task")
    );
    state.input(Input::Resume, Instant::now());
    assert_eq!(state.dataset, Some("a".repeat(32)));
    state.input(Input::Stop, Instant::now());
    assert_eq!(state.snapshot.state, RunState::Stopping);
}

#[test]
fn cost_limit_requires_known_prices_and_storage_ceiling_is_live() {
    let mut state = configured(Arc::new(Stub::default()));
    state.input(
        Input::Limit {
            field: "max_cost_usd".into(),
            value: "10".into(),
        },
        Instant::now(),
    );
    assert!(state.start_reason().unwrap().contains("prices"));
    state.input(
        Input::Limit {
            field: "max_cost_usd".into(),
            value: String::new(),
        },
        Instant::now(),
    );
    state.context.as_mut().unwrap().storage.free_disk = Some(1);
    assert!(state.start_reason().unwrap().contains("storage"));
}
#[test]
fn local_apple_intelligence_tasks_have_zero_model_cost_without_saved_prices() {
    let mut state = configured(Arc::new(Stub::default()));
    state.settings.provider = Some(ai::ProviderId::AppleFoundationModels);
    state.input(
        Input::Limit {
            field: "max_cost_usd".into(),
            value: "0".into(),
        },
        Instant::now(),
    );
    assert!(state.settings.prices.is_empty());
    assert_eq!(state.estimated_cost(), Some((0.0, 0.0)));
    assert!(state.start_reason().is_none());
}

#[test]
fn starting_is_explicit_stop_is_cooperative_and_settings_stay_bound() {
    let stub = Arc::new(Stub::default());
    stub.hold.store(true, Ordering::SeqCst);
    let mut state = configured(stub.clone());
    assert_eq!(stub.calls.load(Ordering::SeqCst), 0);
    state.input(Input::Start, Instant::now());
    drive(&mut state, |s| !s.snapshot.run_id.is_empty());
    assert!(state.active());
    let changed = Settings {
        provider: Some(ai::ProviderId::OpenAi),
        ..Settings::default()
    };
    state.configure(changed, stub.clone());
    assert_eq!(state.settings.provider, Some(ai::ProviderId::Anthropic));
    state.input(Input::Stop, Instant::now());
    drive(&mut state, |s| s.running.is_none());
    assert_eq!(
        state.outcome.as_ref().unwrap().outcome,
        run::Outcome::Stopped
    );
    let req = stub.requests.lock().unwrap();
    assert_eq!(req.len(), 1);
    assert_eq!(req[0].mode, RunMode::Supervised);
    assert!(!req[0].scope.allow_images);
    assert!(!req[0].scope.effects_may_outlive_run);
}
#[test]
fn take_over_waits_for_completion_and_new_task_resets_mode_and_consent() {
    let stub = Arc::new(Stub::default());
    stub.hold.store(true, Ordering::SeqCst);
    let mut state = configured(stub);
    state.allow_images = true;
    state.input(Input::Start, Instant::now());
    drive(&mut state, |s| !s.snapshot.run_id.is_empty());
    assert!(matches!(
        state.input(
            Input::TakeOver(TakeoverTarget::Camera("sim:0".into())),
            Instant::now()
        ),
        Effect::None
    ));
    drive(&mut state, |s| s.ready_takeover.is_some());
    assert!(!state.active());
    assert_eq!(
        state.take_takeover(),
        Some(TakeoverTarget::Camera("sim:0".into()))
    );
    assert!(matches!(
        state.input(Input::NewTask, Instant::now()),
        Effect::ModeChanged(Mode::Plan)
    ));
    assert!(!state.allow_images);
    assert!(state.outcome.is_none());
}
#[test]
fn synthetic_scenes_do_not_start_workers_even_when_inputs_are_replayed() {
    let stub = Arc::new(Stub::default());
    let mut state = configured(stub.clone());
    state.synthetic = true;
    state.input(Input::Open(true), Instant::now());
    state.input(Input::Start, Instant::now());
    state.tick(Instant::now());
    assert_eq!(stub.calls.load(Ordering::SeqCst), 0);
    assert!(!state.active());
}
#[test]
fn crash_scan_and_log_reads_only_happen_after_explicit_inputs() {
    let stub = Arc::new(Stub::default());
    let mut state = State::with_services(stub.clone());
    state.tick(Instant::now());
    assert_eq!(stub.calls.load(Ordering::SeqCst), 0);
    state.input(Input::Open(true), Instant::now());
    drive(&mut state, |s| s.job.is_none());
    assert_eq!(stub.calls.load(Ordering::SeqCst), 1);
    state.crash = Some(CrashRow {
        run_id: "r-20261018-123456-abcd".into(),
        time_ms: 0,
    });
    state.input(Input::ViewLog, Instant::now());
    drive(&mut state, |s| s.log_text.is_some());
    assert!(state.log_text.as_ref().unwrap().contains("Synthetic"));
}
#[test]
fn recovery_refuses_replaced_recordings_and_reconnected_workers() {
    let mut ctx = fixture_context(fixture_snapshot());
    let camera = &mut ctx.snapshot.cameras[0];
    let id = camera.info.id.clone();
    camera.forwarding = Some("same.mkv".into());
    camera.forwarding_id = Some("replacement".into());
    let mut record = run::RunRecord::default();
    record.worker_pids.insert(id.clone(), camera.worker_pid);
    record.forwarders.insert(id.clone(), "original".into());
    let left = run::Leftover {
        kind: "recording".into(),
        camera: id.clone(),
        summary: "Synthetic recording".into(),
        job_id: None,
        until_ms: None,
    };
    assert!(worker::cleanup_plan(&record, &left, &ctx).is_err());
    ctx.snapshot.cameras[0].forwarding_id = Some("original".into());
    assert!(worker::cleanup_plan(&record, &left, &ctx).is_ok());
    ctx.snapshot.cameras[0].worker_pid += 1;
    assert!(worker::cleanup_plan(&record, &left, &ctx).is_err());
}

#[test]
fn waiting_and_approval_time_advances_without_an_active_animation_loop() {
    let mut state = configured(Arc::new(Stub::default()));
    let (sender, events) = std::sync::mpsc::sync_channel(4);
    let (approver, approvals) = worker::GuiApprover::new();
    state.running = Some(worker::Running {
        control: run::RunControl::default(),
        events,
        approver,
        approvals,
        done: Arc::new(AtomicBool::new(false)),
    });
    let now = Instant::now();
    assert!(
        sender
            .send(worker::Event::State(Box::new(run::RunSnapshot {
                state: RunState::WaitingApproval,
                elapsed_ms: 5000,
                ..Default::default()
            })))
            .is_ok()
    );
    state.tick(now);
    assert!(!state.busy());
    state.tick(now + Duration::from_secs(2));
    assert!(state.snapshot.elapsed_ms >= 7000);
    assert!(
        sender
            .send(worker::Event::State(Box::new(run::RunSnapshot {
                state: RunState::Waiting,
                elapsed_ms: 7000,
                ..Default::default()
            })))
            .is_ok()
    );
    state.tick(now + Duration::from_secs(2));
    assert!(!state.busy());
    state.tick(now + Duration::from_secs(4));
    assert!(state.snapshot.elapsed_ms >= 9000);
    state.tick(now + Duration::from_secs(2 * 86400));
    assert_eq!(state.snapshot.elapsed_ms, state.limits.max_wall_ms);
    state.finish_app_quit();
    assert!(!state.active());
}

#[test]
fn metrics_line_uses_the_actual_flat_frame_measurements() {
    let summary = metrics_summary(
        &json!({"exposure":{"mean":0.42,"highlights":0.01,"shadows":0.03},"focus":{"laplacian":12.345}}),
    );
    assert_eq!(
        summary,
        "Brightness 42.0% · Highlights 1.0% · Shadows 3.0% · Focus 12.35"
    );
    assert_eq!(
        metrics_summary(&json!({"notes":["Exposure is unavailable for this format."]})),
        "Exposure is unavailable for this format."
    );
}

#[test]
fn a_leftover_settings_action_only_proposes_that_camera() {
    let mut ctx = fixture_context(fixture_snapshot());
    let features = ctx.snapshot.cameras[0].features.clone();
    ctx.snapshot
        .cameras
        .iter_mut()
        .find(|c| c.info.id == "sim:1")
        .unwrap()
        .features = features;
    let mut record = run::RunRecord::default();
    for camera in &ctx.snapshot.cameras {
        record
            .worker_pids
            .insert(camera.info.id.clone(), camera.worker_pid);
        if let Some(feature) = camera.features.iter().find(|f| f.name == "Gain")
            && let Some(last) = feature.value.as_ref()
        {
            record.settings.push(run::SettingChange {
                camera: camera.info.id.clone(),
                feature: "Gain".into(),
                before: "1".into(),
                last: if let Some(s) = last.as_str() {
                    s.to_owned()
                } else {
                    last.to_string()
                },
                needs_review: false,
            });
        }
    }
    assert_eq!(record.settings.len(), 2);
    let camera = record
        .settings
        .first()
        .expect("gain fixture")
        .camera
        .clone();
    let left = run::Leftover {
        kind: "settings".into(),
        camera: camera.clone(),
        summary: "Settings remain changed".into(),
        job_id: None,
        until_ms: None,
    };
    let plan = worker::cleanup_plan(&record, &left, &ctx).unwrap();
    assert!(!plan.calls.is_empty());
    assert!(plan.calls.iter().all(|call| call.args["camera"] == camera));
}

#[test]
fn asynchronously_loaded_limit_defaults_apply_until_the_user_edits_setup() {
    let stub = Arc::new(Stub::default());
    let mut state = State::with_services(stub.clone());
    state.configure(Settings::default(), stub.clone());
    let mut loaded = Settings {
        autonomy: ai::settings::Autonomy {
            limits: ai::Limits {
                max_rounds: 15,
                ..ai::Limits::default()
            },
            ..Default::default()
        },
        ..Settings::default()
    };
    state.configure(loaded.clone(), stub.clone());
    assert_eq!(state.limits.max_rounds, 15);
    state.input(
        Input::Limit {
            field: "max_rounds".into(),
            value: "20".into(),
        },
        Instant::now(),
    );
    loaded.autonomy.limits.max_rounds = 10;
    state.configure(loaded, stub);
    assert_eq!(state.limits.max_rounds, 20);
    assert_eq!(state.limit_drafts["max_rounds"], "20");
}

#[test]
fn failed_started_tasks_keep_their_log_and_require_new_task_before_editing() {
    let mut state = State::with_services(Arc::new(worker::Unavailable));
    state.snapshot.run_id = "r-20261018-123456-abcd".into();
    state.snapshot.state = RunState::Failed;
    assert!(!state.editable());
    assert_eq!(state.log_id().as_deref(), Some("r-20261018-123456-abcd"));
    assert!(matches!(
        state.input(Input::Mode(Mode::Autonomous), Instant::now()),
        Effect::None
    ));
    assert!(matches!(
        state.input(Input::NewTask, Instant::now()),
        Effect::ModeChanged(Mode::Plan)
    ));
    assert!(state.editable());
    assert!(state.log_id().is_none());
}

#[test]
fn cancellation_before_a_journal_starts_is_stopped_without_an_error() {
    let mut state = State::with_services(Arc::new(worker::Unavailable));
    let (sender, events) = std::sync::mpsc::sync_channel(1);
    let (approver, approvals) = worker::GuiApprover::new();
    let control = run::RunControl::default();
    control.stop();
    state.running = Some(worker::Running {
        control,
        events,
        approver,
        approvals,
        done: Arc::new(AtomicBool::new(true)),
    });
    assert!(
        sender
            .send(worker::Event::Complete(Err(worker::Failure {
                message: "The assistant task was stopped.".into(),
                excerpt: None
            })))
            .is_ok()
    );
    state.tick(Instant::now());
    assert_eq!(state.snapshot.state, RunState::Stopped);
    assert!(state.error.is_none());
    assert!(!state.active());
    assert!(state.editable());
}
