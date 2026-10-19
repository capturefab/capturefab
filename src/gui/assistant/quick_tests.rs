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
    fn test_s3(&self, _: &Checked, _: &dyn SecretSource) -> Result<String, String> {
        Ok("The test bucket is available.".into())
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
fn local_source_bindings_need_consent_survive_refresh_and_follow_ups_and_clear_with_conversation() {
    let services = Arc::new(Stub::new());
    let mut state = state(services.clone());
    let uri = "mqtt://line1.example/private-topic-in-local-binding";
    state.input(Input::DataSourceDraft(uri.into()), Instant::now());
    let revision = state.plan.revision;
    state.input(Input::AddDataSource, Instant::now());
    assert_eq!(state.plan.revision, revision + 1);
    assert_eq!(state.data_sources.len(), 1);
    assert!(state.context.as_ref().unwrap().data_sources.is_empty());
    let source = state.data_sources[0].clone();
    state.input(Input::DataSourceDraft(uri.into()), Instant::now());
    let revision = state.plan.revision;
    state.input(Input::AddDataSource, Instant::now());
    assert_eq!(
        state.plan.revision, revision,
        "duplicate Add does not change the reviewed scope"
    );
    state.input(Input::SeeData(true), Instant::now());
    assert_eq!(state.plan.revision, revision + 1);
    assert_eq!(
        state.context.as_ref().unwrap().data_sources,
        vec![source.clone()]
    );
    state.refresh(Instant::now());
    wait(&mut state, |state| state.gathering.is_none());
    assert_eq!(
        state.context.as_ref().unwrap().data_sources,
        vec![source.clone()]
    );
    for prompt in ["Plan a source reading.", "Review the same source again."] {
        state.prompt = prompt.into();
        state.input(Input::Send, Instant::now());
        wait(&mut state, |state| !state.thinking());
        let seen = services.seen.lock().unwrap();
        let request = seen.last().unwrap();
        assert!(request.allow_data);
        assert_eq!(request.data_sources, vec![source.clone()]);
        assert!(!format!("{request:?}").contains("private-topic-in-local-binding"));
    }
    state.input(Input::SeeData(false), Instant::now());
    assert!(state.conversation.is_none());
    assert!(state.context.as_ref().unwrap().data_sources.is_empty());
    assert_eq!(state.data_sources, vec![source]);
    state.input(
        Input::DataSourceDraft("sim-signal://draft".into()),
        Instant::now(),
    );
    state.input(Input::NewConversation, Instant::now());
    assert!(!state.allow_data);
    assert!(state.data_sources.is_empty());
    assert!(state.data_source_draft.is_empty());
    assert!(!state.data_sources_open);
    assert!(state.context.as_ref().unwrap().data_sources.is_empty());
}

#[test]
fn protected_bindings_need_fresh_context_and_profile_edits_expire_the_review_once() {
    let services = Arc::new(Stub::new());
    let source =
        crate::gui::source_setup::tests::bound_context(&mut services.context.lock().unwrap());
    let mut state = state(services.clone());
    state.allow_data = true;
    state.conversation = Some(conversation());
    let revision = state.plan.revision;
    state.source_effects(vec![crate::gui::source_setup::Effect::Bound(
        source.clone(),
    )]);
    assert_eq!(state.plan.revision, revision + 1);
    assert!(state.conversation.is_none());
    assert!(
        state.context.is_none(),
        "a credential receipt is never treated as a fresh session snapshot"
    );
    assert!(!state.ready_to_run());
    wait(&mut state, |state| state.gathering.is_none());
    let ctx = state.context.as_ref().unwrap();
    assert_eq!(ctx.data_sources, vec![source.clone()]);
    state
        .plan
        .propose(
            ToolCall {
                tool: "read_data_source".into(),
                args: json!({"source":source.reference,"timeout":"1s"}),
            },
            None,
        )
        .unwrap();
    state.recheck();
    assert!(state.ready_to_run(), "{:?}", state.plan.steps[0].checked);
    let revision = state.plan.revision;
    state.source_effects(vec![crate::gui::source_setup::Effect::Invalidate(
        source.profile_id().unwrap().into(),
    )]);
    assert_eq!(state.plan.revision, revision + 1);
    assert!(!state.ready_to_run());
    state.source_effects(vec![crate::gui::source_setup::Effect::Invalidate(
        source.profile_id().unwrap().into(),
    )]);
    assert_eq!(
        state.plan.revision,
        revision + 1,
        "one revoked binding invalidates one review"
    );
    state.input(Input::Run(revision), Instant::now());
    assert_eq!(services.applies.load(Ordering::SeqCst), 0);
    state.input(Input::NewConversation, Instant::now());
    assert!(state.data_sources.is_empty());
}

#[test]
fn a_filled_profile_draft_blocks_send_and_run_until_the_editor_is_closed() {
    let services = Arc::new(Stub::new());
    let mut state = state(services.clone());
    ready_plan(&mut state, 1);
    state.source_setup.fixture();
    state.prompt = "Take one more photo.".into();
    assert!(!state.ready_to_run());
    state.input(Input::Send, Instant::now());
    assert_eq!(services.turns.load(Ordering::SeqCst), 0);
    assert_eq!(
        state.error.as_deref(),
        Some(crate::gui::source_setup::UNCOMMITTED_DRAFT)
    );
    assert_eq!(state.prompt, "Take one more photo.");
    state.input(
        Input::SourceProfile(crate::gui::source_setup::Input::Open(false)),
        Instant::now(),
    );
    assert!(state.ready_to_run());
    state.input(Input::Send, Instant::now());
    wait(&mut state, |state| !state.thinking());
    assert_eq!(services.turns.load(Ordering::SeqCst), 1);
    assert!(services.seen.lock().unwrap()[0].data_sources.is_empty());
}

#[test]
fn source_binding_removal_invalidates_a_reviewed_fresh_read_once_and_refuses_stale_run() {
    let services = Arc::new(Stub::new());
    let mut state = state(services.clone());
    state.input(
        Input::DataSourceDraft("mqtt://line1.example/temperature".into()),
        Instant::now(),
    );
    state.input(Input::AddDataSource, Instant::now());
    state.input(Input::SeeData(true), Instant::now());
    let source = state.data_sources[0].clone();
    let id = state
        .plan
        .propose(
            ToolCall {
                tool: "read_data_source".into(),
                args: json!({"source":source.reference,"timeout":"1s"}),
            },
            None,
        )
        .unwrap();
    state.recheck();
    assert!(state.ready_to_run(), "{:?}", state.plan.steps[0].checked);
    let revision = state.plan.revision;
    state.input(Input::RemoveDataSource(source.reference), Instant::now());
    assert_eq!(state.plan.revision, revision + 1);
    assert!(!state.ready_to_run());
    assert!(
        state
            .plan
            .steps
            .iter()
            .find(|step| step.id == id)
            .unwrap()
            .checked
            .as_ref()
            .unwrap()
            .ops
            .is_empty()
    );
    state.input(Input::Run(revision), Instant::now());
    assert_eq!(services.applies.load(Ordering::SeqCst), 0);
}

#[test]
fn credentialed_source_entries_are_refused_and_source_messages_redact_drafts() {
    let mut state = state(Arc::new(Stub::new()));
    for uri in [
        "mqtt://user:private-binding-password@host/topic",
        "mqtt://host/topic?token=private-binding-token",
        "mqtt://user@host/topic",
    ] {
        let message = Input::DataSourceDraft(uri.into());
        assert!(!format!("{message:?}").contains(uri));
        state.input(message, Instant::now());
        assert!(state.source_draft_error().is_some());
        state.input(Input::AddDataSource, Instant::now());
        assert!(state.data_sources.is_empty());
        assert!(!state.error.as_ref().unwrap().contains("private-binding"));
    }
}

#[test]
fn a_busy_source_turn_cannot_change_bindings_or_data_consent_and_late_replies_stay_stopped() {
    let gate = Arc::new(Gate::default());
    let mut stub = Stub::new();
    stub.gate = Some(gate.clone());
    let services = Arc::new(stub);
    let mut state = state(services);
    state.input(
        Input::DataSourceDraft("sim-signal://line1".into()),
        Instant::now(),
    );
    state.input(Input::AddDataSource, Instant::now());
    state.input(Input::SeeData(true), Instant::now());
    let source = state.data_sources[0].clone();
    state.prompt = "Review the approved source.".into();
    state.input(Input::Send, Instant::now());
    wait(&mut state, |state| state.plan.steps.len() == 1);
    let revision = state.plan.revision;
    let generation = state.generation;
    for input in [
        Input::DataSourceDraft("sim-signal://line2".into()),
        Input::AddDataSource,
        Input::RemoveDataSource(source.reference.clone()),
        Input::SeeData(false),
    ] {
        state.input(input, Instant::now());
    }
    assert_eq!(state.plan.revision, revision);
    assert_eq!(state.generation, generation);
    assert_eq!(state.data_sources, vec![source.clone()]);
    assert!(state.allow_data);
    state.input(Input::Stop, Instant::now());
    gate.release.store(true, Ordering::SeqCst);
    wait(&mut state, |state| !state.thinking());
    assert!(state.plan.steps.is_empty());
    state.input(Input::RemoveDataSource(source.reference), Instant::now());
    assert!(state.data_sources.is_empty());
    assert!(state.generation > generation);
}

#[test]
fn string_list_add_edit_remove_preserves_json_and_focus_without_executing() {
    let services = Arc::new(Stub::new());
    let mut state = state(services.clone());
    for (tool, key) in [
        ("start_signal_capture", "tags"),
        ("export_dataset", "tables"),
        ("get_camera_features", "names"),
    ] {
        let id = state
            .plan
            .propose(
                ToolCall {
                    tool: tool.into(),
                    args: json!({key:null}),
                },
                None,
            )
            .unwrap();
        let edit = |remove| Input::StringList {
            step: id,
            key: key.into(),
            remove,
        };
        for (index, name) in ["first", "second"].into_iter().enumerate() {
            let effect = state.input(edit(None), Instant::now());
            assert!(
                matches!(effect,Effect::FocusField(ref field) if field == &format!("step-{}-{key}.{index}",id.0))
            );
            state.input(
                Input::Field {
                    step: id,
                    key: format!("{key}.{index}"),
                    value: name.into(),
                },
                Instant::now(),
            );
        }
        let effect = state.input(edit(Some(0)), Instant::now());
        assert!(
            matches!(effect,Effect::FocusField(ref field) if field == &format!("step-{}-{key}.0",id.0))
        );
        let step = state.plan.steps.iter().find(|step| step.id == id).unwrap();
        assert_eq!(step.call.args[key], json!(["second"]));
        assert_eq!(
            state.drafts.text(id, &format!("{key}.0"), &step.call.args),
            "second"
        );
        let effect = state.input(edit(Some(0)), Instant::now());
        assert!(
            matches!(effect,Effect::FocusField(ref field) if field == &format!("step-{}-{key}-add",id.0))
        );
        assert_eq!(
            state
                .plan
                .steps
                .iter()
                .find(|step| step.id == id)
                .unwrap()
                .call
                .args[key],
            json!([])
        );
        assert!(matches!(
            state.input(edit(Some(0)), Instant::now()),
            Effect::None
        ));
    }
    assert_eq!(services.applies.load(Ordering::SeqCst), 0);
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
fn exporting_a_fresh_source_plan_is_portable_without_changing_the_model_plan() {
    let services = Arc::new(Stub::new());
    let mut state = state(services.clone());
    let uri = "sim-signal://exported-from-quick";
    state.input(Input::DataSourceDraft(uri.into()), Instant::now());
    state.input(Input::AddDataSource, Instant::now());
    state.input(Input::SeeData(true), Instant::now());
    let source = state.data_sources[0].clone();
    state
        .plan
        .propose(
            ToolCall {
                tool: "read_data_source".into(),
                args: json!({"source":source.reference,"timeout":"1s"}),
            },
            None,
        )
        .unwrap();
    state.recheck();
    assert!(state.ready_to_run(), "{:?}", state.plan.value());
    let original = state.plan.value();
    state.input(Input::ExportPlan, Instant::now());
    wait(&mut state, |state| state.exporting.is_none());
    let exported = services.exported.lock().unwrap()[0].clone();
    assert_eq!(state.plan.value(), original, "export is not a plan edit");
    assert_eq!(
        state.plan.document().calls[0].args["source"],
        source.reference
    );
    assert_eq!(exported.calls[0].args["source"], uri);
    let exported = tools::plan::parse(&tools::plan::to_json(&exported).unwrap()).unwrap();
    let mut fresh_cli = fixture_context(fixture_snapshot());
    fresh_cli.data_sources.clear();
    fresh_cli.snapshot.signals.clear();
    fresh_cli.snapshot.signal_bindings.clear();
    let calls = exported
        .calls
        .into_iter()
        .map(|call| (StepId(1), call))
        .collect::<Vec<_>>();
    let dry_run = tools::check_plan(&fresh_cli, &calls, &|_, _| false);
    assert!(dry_run.iter().all(Checked::ready), "{dry_run:?}");
    assert!(dry_run[0].ops.iter().any(|operation|matches!(operation,tools::Op::Session {command:crate::session::SessionCommand::SignalRead {uri:target,auth:None,..},..} if target==uri)));
    assert_eq!(services.applies.load(Ordering::SeqCst), 0);

    // A later export click uses the current approved scope, not the source
    // list from the earlier successful file export.
    state.input(Input::RemoveDataSource(source.reference), Instant::now());
    state.input(Input::ExportPlan, Instant::now());
    assert!(state.exporting.is_none());
    assert!(
        state
            .error
            .as_deref()
            .is_some_and(|error| error.contains("Sources"))
    );
    assert_eq!(services.exported.lock().unwrap().len(), 1);
}

#[test]
fn exporting_a_protected_source_requires_local_rebinding_and_never_exports_its_epoch() {
    let services = Arc::new(Stub::new());
    let mut state = state(services.clone());
    let mut source = DataSourceRef::approved("mqtts://host/private-topic").unwrap();
    source.auth = Some(crate::signals::auth::AuthBinding {
        profile: crate::signals::auth::ProfileRef {
            id: "a".repeat(32),
            revision: "b".repeat(32),
        },
        epoch: "c".repeat(32),
    });
    state.data_sources.push(source.clone());
    state
        .plan
        .propose(
            ToolCall {
                tool: "read_data_source".into(),
                args: json!({"source":source.reference,"timeout":"1s"}),
            },
            None,
        )
        .unwrap();
    let original = state.plan.document();
    state.input(Input::ExportPlan, Instant::now());
    assert!(state.exporting.is_none());
    assert!(services.exported.lock().unwrap().is_empty());
    let error = state.error.as_deref().unwrap();
    assert!(
        error.contains("Protected") && error.contains("in the app") && error.contains("bind"),
        "{error}"
    );
    assert!(!error.contains(&"c".repeat(32)));
    assert_eq!(state.plan.document(), original);
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
