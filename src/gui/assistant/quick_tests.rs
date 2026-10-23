use super::*;
use crate::{
    assistant::{
        ProviderId,
        agent::{PlanOutcome, PlanRequest},
    },
    tools::{
        ApplyError, Progress, SecretSource,
        tests::{fixture_context, fixture_snapshot},
    },
};
use std::sync::atomic::AtomicUsize;

#[derive(Default)]
struct Gate {
    entered: AtomicBool,
    release: AtomicBool,
    cancel_seen: AtomicBool,
}
struct Stub {
    context: Mutex<Context>,
    gathers: AtomicUsize,
    turns: AtomicUsize,
    applies: AtomicUsize,
    gate: Option<Arc<Gate>>,
    partial: bool,
    panic_after_first: bool,
    fail_turn: bool,
    seen: Mutex<Vec<PlanRequest>>,
    exported: Mutex<Vec<tools::plan::PlanDocument>>,
}
impl Stub {
    fn new() -> Self {
        Self {
            context: Mutex::new(fixture_context(fixture_snapshot())),
            gathers: AtomicUsize::new(0),
            turns: AtomicUsize::new(0),
            applies: AtomicUsize::new(0),
            gate: None,
            partial: false,
            panic_after_first: false,
            fail_turn: false,
            seen: Mutex::new(vec![]),
            exported: Mutex::new(vec![]),
        }
    }
}
fn photo(name: &str) -> ToolCall {
    ToolCall {
        tool: "capture_photos".into(),
        args: json!({"camera":"sim:0","count":1,"format":"png","file_name":format!("{name}-{{seq}}.png"),"destination":null,"tags":null}),
    }
}
fn conversation() -> Conversation {
    Conversation::new(ProviderId::Anthropic, "test-model", "A test.", vec![])
}
impl Services for Stub {
    fn gather(&self) -> anyhow::Result<Context> {
        self.gathers.fetch_add(1, Ordering::SeqCst);
        Ok(self.context.lock().unwrap().clone())
    }
    fn turn(
        &self,
        _: Settings,
        request: PlanRequest,
        _: Arc<dyn SecretSource + Send + Sync>,
        events: &mut dyn FnMut(ai::agent::Event),
        cancel: &AtomicBool,
    ) -> anyhow::Result<PlanOutcome> {
        self.turns.fetch_add(1, Ordering::SeqCst);
        self.seen.lock().unwrap().push(request.clone());
        anyhow::ensure!(
            !self.fail_turn,
            "The test provider is temporarily unavailable."
        );
        let mut plan = request.plan.clone();
        plan.propose(photo("from-model"), None)?;
        events(ai::agent::Event::Plan(plan.clone()));
        if let Some(gate) = &self.gate {
            gate.entered.store(true, Ordering::SeqCst);
            let until = Instant::now() + Duration::from_secs(3);
            while !gate.release.load(Ordering::SeqCst) {
                if cancel.load(Ordering::SeqCst) {
                    gate.cancel_seen.store(true, Ordering::SeqCst);
                }
                anyhow::ensure!(Instant::now() < until, "Test turn was not released.");
                std::thread::sleep(Duration::from_millis(1));
            }
            // Deliberately ignore cancellation to exercise late-reply guards.
            plan.propose(photo("late-model-step"), None)?;
            events(ai::agent::Event::Plan(plan.clone()));
        }
        Ok(PlanOutcome {
            text: "A capture is ready for review.".into(),
            plan,
            conversation: conversation(),
            usage: Usage {
                input: 10,
                output: 5,
                ..Usage::default()
            },
            cancelled: false,
            error: None,
            run_id: "test-run".into(),
            log: PathBuf::new(),
        })
    }
    fn apply(
        &self,
        calls: &[(StepId, ToolCall)],
        _: &[Checked],
        _: &dyn SecretSource,
        progress: &mut dyn FnMut(Progress),
        cancel: &AtomicBool,
    ) -> Result<Value, ApplyError> {
        self.applies.fetch_add(1, Ordering::SeqCst);
        let mut completed = vec![];
        for (index, _) in calls.iter().enumerate() {
            if self.partial && index == 1 {
                if let Some(gate) = &self.gate {
                    gate.entered.store(true, Ordering::SeqCst);
                    let until = Instant::now() + Duration::from_secs(3);
                    while !cancel.load(Ordering::SeqCst) && !gate.release.load(Ordering::SeqCst) {
                        assert!(Instant::now() < until);
                        std::thread::sleep(Duration::from_millis(1));
                    }
                }
                return Err(ApplyError::Run(tools::PlanFailure {
                    source: anyhow::anyhow!("The plan was stopped."),
                    completed,
                    failed_step: index + 1,
                    step_count: calls.len(),
                }));
            }
            progress(Progress {
                step: index + 1,
                step_count: calls.len(),
                state: ProgressState::Running,
                summary: format!("Capture {}", index + 1),
            });
            completed.push(
                json!({"summary":format!("Saved capture {}.",index+1),"results":[],"watch":[]}),
            );
            progress(Progress {
                step: index + 1,
                step_count: calls.len(),
                state: ProgressState::Completed,
                summary: format!("Saved capture {}.", index + 1),
            });
            assert!(
                !self.panic_after_first,
                "Simulated worker failure after an acknowledged step."
            );
        }
        Ok(json!({"completed":completed}))
    }
    fn export(&self, document: &tools::plan::PlanDocument) -> Result<Option<PathBuf>, String> {
        self.exported.lock().unwrap().push(document.clone());
        Ok(Some(PathBuf::from("/tmp/offline-plan.json")))
    }
}
fn state(services: Arc<Stub>) -> State {
    let mut state = State::with_services(services.clone());
    state.context = Some(services.context.lock().unwrap().clone());
    state.settings.provider = Some(ProviderId::Anthropic);
    state
}
fn wait(state: &mut State, condition: impl Fn(&State) -> bool) {
    let until = Instant::now() + Duration::from_secs(5);
    while !condition(state) {
        state.tick(Instant::now());
        assert!(
            Instant::now() < until,
            "Quick command worker did not finish"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}
fn ready_plan(state: &mut State, count: usize) -> Vec<StepId> {
    let ids = (0..count)
        .map(|index| {
            state
                .plan
                .propose(photo(&format!("capture-{index}")), None)
                .unwrap()
        })
        .collect();
    state.recheck();
    assert!(state.ready_to_run(), "{:?}", state.plan.value());
    ids
}

#[test]
fn retry_keeps_the_exact_failed_provider_prompt() {
    let services = Arc::new(Stub {
        fail_turn: true,
        ..Stub::new()
    });
    let mut state = state(services.clone());
    state.input(Input::Prompt("Review this camera.".into()), Instant::now());
    state.input(Input::Send, Instant::now());
    wait(&mut state, |state| !state.thinking());
    assert!(state.retry_available());
    state.input(Input::Retry, Instant::now());
    wait(&mut state, |state| !state.thinking());
    assert_eq!(services.turns.load(Ordering::SeqCst), 2);
    assert!(
        services
            .seen
            .lock()
            .unwrap()
            .iter()
            .all(|request| request.prompt == "Review this camera.")
    );
}

#[test]
fn a_default_model_is_inert_and_scoped_to_an_explicit_send() {
    let initial = State::default();
    assert!(!initial.busy());
    assert!(!initial.open);
    assert!(!initial.allow_images);
    assert!(!initial.has_provider());
    let services = Arc::new(Stub::new());
    let mut state = state(services.clone());
    state.settings.provider = None;
    state.prompt = "Take one photo.".into();
    state.input(Input::Send, Instant::now());
    assert_eq!(services.turns.load(Ordering::SeqCst), 0);
    assert!(state.error.is_some());
    assert!(state.transcript.is_empty());
}
#[test]
fn closing_and_reopening_a_turn_never_cancels_it_or_discards_its_plan() {
    let gate = Arc::new(Gate::default());
    let mut stub = Stub::new();
    stub.gate = Some(gate.clone());
    let services = Arc::new(stub);
    let mut state = state(services.clone());
    state.prompt = "Take one photo.".into();
    state.input(Input::Send, Instant::now());
    wait(&mut state, |state| !state.plan.steps.is_empty());
    state.input(Input::Open(false), Instant::now());
    assert!(state.thinking());
    assert!(!state.turn.as_ref().unwrap().cancel.load(Ordering::SeqCst));
    state.input(Input::Open(true), Instant::now());
    assert!(state.thinking());
    gate.release.store(true, Ordering::SeqCst);
    wait(&mut state, |state| !state.thinking());
    assert_eq!(state.plan.steps.len(), 2);
    assert_eq!(services.turns.load(Ordering::SeqCst), 1);
    assert!(state.conversation.is_some());
}
#[test]
fn stop_restores_the_pre_turn_plan_and_rejects_all_late_model_proposals() {
    let gate = Arc::new(Gate::default());
    let mut stub = Stub::new();
    stub.gate = Some(gate.clone());
    let services = Arc::new(stub);
    let mut state = state(services.clone());
    let initial = ready_plan(&mut state, 1);
    let old = state.plan.calls();
    state.prompt = "Also take another photo.".into();
    state.input(Input::Send, Instant::now());
    wait(&mut state, |state| state.plan.steps.len() > initial.len());
    let revision = state.plan.revision;
    state.input(Input::Run(revision), Instant::now());
    state.input(Input::Remove(initial[0]), Instant::now());
    assert_eq!(services.applies.load(Ordering::SeqCst), 0);
    state.input(Input::Stop, Instant::now());
    assert_eq!(state.plan.calls(), old);
    assert!(state.plan.revision > revision);
    assert!(state.conversation.is_none());
    state.prompt = "This cannot overlap.".into();
    state.input(Input::Send, Instant::now());
    assert_eq!(services.turns.load(Ordering::SeqCst), 1);
    gate.release.store(true, Ordering::SeqCst);
    wait(&mut state, |state| !state.thinking());
    assert_eq!(state.plan.calls(), old);
    assert!(state.conversation.is_none());
    assert_eq!(state.status, "Stopped");
    let id = state.plan.propose(photo("after-stop"), None).unwrap();
    assert!(id.0 > 3, "Even ignored late proposals reserve their IDs.");
}
#[test]
fn follow_up_requests_carry_the_edited_plan_and_opt_in_images_only() {
    let services = Arc::new(Stub::new());
    let mut state = state(services.clone());
    let ids = ready_plan(&mut state, 1);
    state.input(
        Input::Field {
            step: ids[0],
            key: "count".into(),
            value: "2".into(),
        },
        Instant::now(),
    );
    state.prompt = "Check this plan.".into();
    state.input(Input::Send, Instant::now());
    wait(&mut state, |state| !state.thinking());
    assert!(!services.seen.lock().unwrap()[0].allow_images);
    assert_eq!(
        services.seen.lock().unwrap()[0].plan.steps[0].call.args["count"],
        2
    );
    state.input(Input::SeeImages(true), Instant::now());
    state.prompt = "Inspect the current view.".into();
    state.input(Input::Send, Instant::now());
    wait(&mut state, |state| !state.thinking());
    let seen = services.seen.lock().unwrap();
    assert!(seen[1].allow_images);
    assert!(seen[1].conversation.is_some());
}
#[test]
fn invalid_edits_and_a_stale_displayed_revision_block_every_effect() {
    let services = Arc::new(Stub::new());
    let mut state = state(services.clone());
    let id = ready_plan(&mut state, 1)[0];
    let old = state.plan.revision;
    state.input(
        Input::Field {
            step: id,
            key: "count".into(),
            value: "ten".into(),
        },
        Instant::now(),
    );
    assert!(!state.ready_to_run());
    state.input(Input::Run(old), Instant::now());
    assert_eq!(services.applies.load(Ordering::SeqCst), 0);
    state.input(
        Input::Field {
            step: id,
            key: "count".into(),
            value: "2".into(),
        },
        Instant::now(),
    );
    assert!(state.ready_to_run());
    state.input(Input::Run(old), Instant::now());
    assert_eq!(services.applies.load(Ordering::SeqCst), 0);
    assert!(
        state
            .error
            .as_deref()
            .unwrap()
            .contains("changed since you reviewed")
    );
    let current = state.plan.revision;
    state.input(Input::Run(current), Instant::now());
    wait(&mut state, |state| !state.running());
    assert_eq!(services.applies.load(Ordering::SeqCst), 1);
    assert!(state.plan.steps.is_empty());
}
#[test]
fn an_apply_stop_keeps_completed_effects_and_leaves_only_remaining_steps() {
    let gate = Arc::new(Gate::default());
    let mut stub = Stub::new();
    stub.gate = Some(gate.clone());
    stub.partial = true;
    let services = Arc::new(stub);
    let mut state = state(services.clone());
    let ids = ready_plan(&mut state, 2);
    let revision = state.plan.revision;
    state.input(Input::Run(revision), Instant::now());
    wait(&mut state, |state| {
        gate.entered.load(Ordering::SeqCst) && state.step_label(ids[0]) == Some("Done")
    });
    state.input(Input::Open(false), Instant::now());
    assert!(state.running());
    assert!(
        !state
            .applying
            .as_ref()
            .unwrap()
            .cancel
            .load(Ordering::SeqCst)
    );
    state.input(Input::Stop, Instant::now());
    wait(&mut state, |state| !state.running());
    assert_eq!(state.plan.steps.len(), 1);
    assert_eq!(state.plan.steps[0].id, ids[1]);
    assert_eq!(state.run_label(), "Run remaining steps");
    assert!(state.transcript.iter().any(|entry| matches!(
        entry,
        TranscriptEntry::Outcome {
            level: Level::Done,
            ..
        }
    )));
    assert_eq!(state.step_label(ids[1]), Some("Failed"));
}
#[test]
fn a_worker_disconnect_never_puts_acknowledged_steps_back_in_the_plan() {
    let mut stub = Stub::new();
    stub.panic_after_first = true;
    let services = Arc::new(stub);
    let mut state = state(services);
    let ids = ready_plan(&mut state, 2);
    let revision = state.plan.revision;
    state.input(Input::Run(revision), Instant::now());
    wait(&mut state, |state| !state.running());
    assert_eq!(state.plan.steps.len(), 1);
    assert_eq!(state.plan.steps[0].id, ids[1]);
    assert!(state.context.is_none());
    assert!(state.error.as_deref().unwrap().contains("worker stopped"));
    assert!(!state.ready_to_run());
}
#[test]
fn each_secret_edit_and_clear_changes_the_displayed_revision() {
    let services = Arc::new(Stub::new());
    let mut state = state(services);
    let id = state
        .plan
        .propose(
            ToolCall {
                tool: "stream_to_url".into(),
                args: json!({"camera":"sim:0","url":"rtsp://stream.example/live","duration":null,"codec":null,"fps":null,"bitrate":null}),
            },
            None,
        )
        .unwrap();
    state.recheck();
    let first = state.plan.revision;
    state.input(
        Input::Secret {
            step: id,
            key: "password".into(),
            value: "first-password".to_owned().into(),
        },
        Instant::now(),
    );
    let second = state.plan.revision;
    assert!(second > first);
    state.input(
        Input::Secret {
            step: id,
            key: "password".into(),
            value: "second-password".to_owned().into(),
        },
        Instant::now(),
    );
    let third = state.plan.revision;
    assert!(third > second);
    state.clear_secrets();
    assert!(state.plan.revision > third);
    assert!(state.drafts.snapshot().secret(id, "password").is_none());
}
#[test]
fn refreshing_camera_facts_invalidates_a_click_from_the_previous_view() {
    let services = Arc::new(Stub::new());
    let mut state = state(services.clone());
    ready_plan(&mut state, 1);
    let previous = state.plan.revision;
    state.context.as_mut().unwrap().capture_root = PathBuf::from("/tmp/a-different-capture-root");
    state.recheck();
    assert!(state.plan.revision > previous);
    state.input(Input::Run(previous), Instant::now());
    assert_eq!(services.applies.load(Ordering::SeqCst), 0);
}
#[test]
fn exports_use_the_worker_and_keep_secrets_out_of_messages_and_plan_documents() {
    let services = Arc::new(Stub::new());
    let mut state = state(services.clone());
    let id = ready_plan(&mut state, 1)[0];
    let secret = "distinct-private-secret-value";
    let input = Input::Secret {
        step: id,
        key: "password".into(),
        value: secret.to_owned().into(),
    };
    assert!(!format!("{input:?}").contains(secret));
    assert!(!format!("{:?}", Input::Prompt(secret.into())).contains(secret));
    assert!(
        !format!(
            "{:?}",
            Input::Field {
                step: id,
                key: "url".into(),
                value: format!("rtsp://u:{secret}@host/x")
            }
        )
        .contains(secret)
    );
    state.input(input, Instant::now());
    state.input(Input::ExportPlan, Instant::now());
    wait(&mut state, |state| state.exporting.is_none());
    assert_eq!(services.exported.lock().unwrap().len(), 1);
    assert!(
        !serde_json::to_string(&services.exported.lock().unwrap()[0])
            .unwrap()
            .contains(secret)
    );
    state.input(Input::Open(false), Instant::now());
    assert!(state.drafts.snapshot().secret(id, "password").is_none());
}

#[test]
fn no_secret_is_copied_to_the_transcript_when_prompt_validation_refuses_it() {
    let services = Arc::new(Stub::new());
    let mut state = state(services.clone());
    state.prompt = "Stream to rtsp://user:secret-password@host.test/video".into();
    state.input(Input::Send, Instant::now());
    assert!(state.transcript.is_empty());
    assert_eq!(services.turns.load(Ordering::SeqCst), 0);
    assert!(state.error.is_some());
}
#[test]
fn transcript_storage_is_bounded_and_keeps_utf8_text_valid() {
    let services = Arc::new(Stub::new());
    let mut state = state(services);
    for _ in 0..300 {
        state.append(TranscriptEntry::Assistant("é".repeat(4096)));
    }
    assert!(state.transcript.len() <= MAX_TRANSCRIPT);
    assert!(transcript_bytes(&state.transcript) <= MAX_TEXT_BYTES);
    assert!(bounded(&"é".repeat(20), 13).is_char_boundary(12));
}
#[test]
fn pure_synthetic_scenes_do_not_call_any_worker_service() {
    let services = Arc::new(Stub::new());
    let mut state = state(services.clone());
    state.synthetic = true;
    state.input(Input::Open(true), Instant::now());
    state.prompt = "Take one photo.".into();
    state.input(Input::Send, Instant::now());
    state.tick(Instant::now() + CONTEXT_INTERVAL);
    assert_eq!(services.gathers.load(Ordering::SeqCst), 0);
    assert_eq!(services.turns.load(Ordering::SeqCst), 0);
    assert!(!state.busy());
}
#[test]
fn new_conversations_reset_image_consent_and_keep_the_selected_provider() {
    let services = Arc::new(Stub::new());
    let mut state = state(services);
    ready_plan(&mut state, 1);
    state.allow_images = true;
    state.input(Input::NewConversation, Instant::now());
    assert!(!state.allow_images);
    assert!(state.plan.steps.is_empty());
    assert!(state.has_provider());
}
