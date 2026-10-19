use super::*;
use crate::{
    assistant::{approval::Approver, run::RunEvent},
    tools::{
        SecretSource,
        tests::{fixture_context, fixture_snapshot},
    },
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
    approval: AtomicBool,
    observed_secret: AtomicBool,
    observed_human: AtomicBool,
    flush_hold: AtomicBool,
    flushing: AtomicBool,
    flush_released: AtomicBool,
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
        approver: Arc<worker::GuiApprover>,
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
        if self.approval.load(Ordering::SeqCst) {
            let ctx = fixture_context(fixture_snapshot());
            let call = ToolCall {
                tool: "create_s3_destination".into(),
                args: json!({"name":"new-archive","bucket":"camera-archive","region":"us-east-1","endpoint":null,"prefix":null,"path_style":null,"credentials":"keychain","profile":null,"access_key_id":"AKIAIOSFODNN7EXAMPLE","keep_local":null}),
            };
            let checked = tools::check(
                &ctx,
                &mut tools::PlanState::new(&ctx),
                StepId(9),
                &call,
                &|_, _| true,
            );
            let req = ApprovalRequest::new(7, "r-20261018-123456-abcd", &checked, u64::MAX);
            let decision = approver.request(req, control.cancel(), Duration::from_secs(5));
            if let ApprovalDecision::Approved { by, .. } = decision {
                self.observed_human.store(
                    by.kind == crate::session::ActorKind::Human,
                    Ordering::SeqCst,
                );
                self.observed_secret.store(
                    approver.secrets.secret(StepId(9), "secret").is_some(),
                    Ordering::SeqCst,
                );
            }
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.hold.load(Ordering::SeqCst) && !control.cancel().load(Ordering::SeqCst) {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        if self.flush_hold.load(Ordering::SeqCst) && control.cancel().load(Ordering::SeqCst) {
            self.flushing.store(true, Ordering::SeqCst);
            while !self.flush_released.load(Ordering::SeqCst) {
                assert!(
                    Instant::now() < deadline,
                    "the fixture's source flush was not released"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
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
            signal_reports: Vec::new(),
            task_data: None,
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
#[cfg(all(unix, feature = "parquet"))]
fn retained_dataset_inventory_confirmation_and_new_task_use_real_local_capabilities() {
    const CHILD: &str = "CAPTUREFAB_GUI_RETAINED_TEST";
    const TEST: &str = "gui::assistant::tasks::tests::retained_dataset_inventory_confirmation_and_new_task_use_real_local_capabilities";
    if std::env::var_os(CHILD).is_none() {
        let root = std::env::temp_dir().join(format!(
            "capturefab-gui-retained-{}-{}",
            std::process::id(),
            wall_ms()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &root)
            .env("CAPTUREFAB_SESSION_DIR", root.join("state"))
            .env("CAPTUREFAB_CACHE_DIR", root.join("cache"))
            .env(CHILD, "1")
            .envs(
                std::env::var_os("CAPTUREFAB_LAYOUT_ARTIFACTS")
                    .map(|path| ("CAPTUREFAB_LAYOUT_ARTIFACTS", path)),
            )
            .args(["--exact", TEST, "--nocapture"])
            .output()
            .unwrap();
        let _ = std::fs::remove_dir_all(root);
        assert!(
            result.status.success(),
            "{} {}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        return;
    }
    let root = PathBuf::from(std::env::var_os("HOME").unwrap());
    let session = crate::session::SessionHandle::new();
    let handle = session
        .begin_task_data(crate::session::task_data::TaskDataPolicy {
            parent: root.join("captures"),
            storage: crate::storage::StoragePolicy::default(),
            max_bytes: 16 << 20,
            lifetime: Duration::from_secs(60),
            name: "GUI retained test".into(),
        })
        .unwrap();
    let path = PathBuf::from(&handle.status().catalog.dataset.path);
    handle
        .note(
            &handle.status().catalog.identity.slot_id,
            "GUI copy survives removal",
            &crate::context::Context::new(),
            None,
        )
        .unwrap();
    handle.close(Duration::from_secs(10)).unwrap();
    let services = Arc::new(worker::NativeServices::new(
        session,
        crate::session::SessionActor::process(crate::session::ActivityOrigin::Gui),
        root.join("captures"),
        crate::storage::StoragePolicy::default(),
        Default::default(),
    ));
    let mut state = State::with_services(services.clone());
    assert!(
        state.retained.is_empty(),
        "constructing state performs no inventory IO"
    );
    state.input(Input::RetainedDatasets, Instant::now());
    drive(&mut state, |state| !state.busy());
    assert_eq!(state.retained.len(), 1);
    assert!(state.retained[0].removable());
    let saved_copy = root.join("saved-copy");
    state.retained[0].copy_to(&saved_copy).unwrap();
    let copy_charge = crate::storage::quota_status().unwrap()["allocated_bytes"]
        .as_u64()
        .unwrap()
        - (16 << 20);
    assert!(copy_charge > 0);
    crate::gui::assistant::task_layout_tests::verify_retained_controls(&state, false);
    let key = state.retained[0].key;
    state.error = Some("An earlier retained operation failed.".into());
    state.input(Input::RemoveRetained(key), Instant::now());
    assert!(
        state.error.is_none(),
        "a fresh review clears stale operation feedback"
    );
    let (_, review) = state.remove_retained.unwrap();
    crate::gui::assistant::task_layout_tests::verify_retained_controls(&state, true);
    assert!(path.exists(), "requesting confirmation never deletes");
    state.input(Input::CancelRemoveRetained, Instant::now());
    state.input(Input::ConfirmRemoveRetained(key, review), Instant::now());
    assert!(
        !state.busy() && path.exists(),
        "cancelled confirmation cannot be replayed"
    );
    state.input(Input::RemoveRetained(key), Instant::now());
    state.input(Input::ConfirmRemoveRetained(key, review), Instant::now());
    assert!(
        path.exists() && !state.busy(),
        "an older confirmation cannot authorize a renewed review"
    );
    let (_, hidden_review) = state.remove_retained.unwrap();
    state.input(Input::Open(false), Instant::now());
    state.input(
        Input::ConfirmRemoveRetained(key, hidden_review),
        Instant::now(),
    );
    assert!(
        state.remove_retained.is_none() && !state.busy() && path.exists(),
        "closing the sheet cancels a permanent-removal review"
    );
    state.input(Input::Open(true), Instant::now());
    drive(&mut state, |state| !state.busy());
    state.input(Input::RemoveRetained(key), Instant::now());
    state.input(Input::NewTask, Instant::now());
    assert!(state.remove_retained.is_none() && path.exists());
    state.input(Input::ConfirmRemoveRetained(key, review), Instant::now());
    assert!(path.exists(), "new task discards the old confirmation");
    state.input(Input::RetainedDatasets, Instant::now());
    drive(&mut state, |state| !state.busy());
    assert_eq!(
        state.retained.len(),
        1,
        "new tasks do not strand prior capabilities"
    );
    state.input(Input::RemoveRetained(key), Instant::now());
    assert!(
        state.remove_retained.is_none(),
        "old inventory events cannot select a refreshed row"
    );
    let key = state.retained[0].key;
    state.input(Input::RemoveRetained(key), Instant::now());
    let (_, review) = state.remove_retained.unwrap();
    state.input(Input::ConfirmRemoveRetained(key, review), Instant::now());
    assert!(state.busy(), "native removal runs off the UI thread");
    drive(&mut state, |state| !state.busy());
    assert!(state.error.is_none(), "{:?}", state.error);
    assert!(!path.exists() && state.retained.is_empty());
    assert_eq!(
        handle.status().phase,
        crate::session::task_data::TaskDataPhase::Removed
    );
    assert_eq!(
        crate::storage::quota_status().unwrap()["allocated_bytes"],
        copy_charge
    );
    let copied = duckdb::Connection::open(saved_copy.join("task.duckdb")).unwrap();
    let text: String = copied
        .query_row("SELECT text FROM events WHERE kind='note'", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(text, "GUI copy survives removal");
}

#[test]
fn task_state_default_is_inert_and_input_debug_redacts_drafts() {
    let stub = Arc::new(Stub::default());
    let state = State::with_services(stub.clone());
    assert!(!state.active());
    assert!(!state.busy());
    assert_eq!(state.mode, Mode::Plan);
    assert!(state.cameras.is_empty());
    assert_eq!(stub.calls.load(Ordering::SeqCst), 0);
    for input in [
        Input::ApprovalSecret(SecretDraft::from("synthetic-private-key".to_owned())),
        Input::Goal("synthetic-private-key".into()),
        Input::ApprovalNote("synthetic-private-key".into()),
        Input::Message("synthetic-private-key".into()),
        Input::DataSourceDraft("synthetic-private-key".into()),
        Input::DataSource("synthetic-private-key".into(), true),
        Input::Limit {
            field: "max_cost_usd".into(),
            value: "synthetic-private-key".into(),
        },
    ] {
        assert!(!format!("{input:?}").contains("synthetic-private-key"));
    }
}

#[test]
fn retained_operation_errors_clear_for_a_new_attempt_without_hiding_invalid_selection_errors() {
    let mut state = State::with_services(Arc::new(Stub::default()));
    state.reply(worker::Reply::Retained(Err(
        "Inventory could not read its storage ledger.".into(),
    )));
    assert!(state.error.as_deref().unwrap().contains("storage ledger"));
    state.input(Input::RetainedDatasets, Instant::now());
    assert!(state.error.is_none() && state.retained_busy());
    drive(&mut state, |state| !state.busy());
    assert!(state.error.is_none() && state.retained.is_empty());
    state.reply(worker::Reply::Copied(Err(
        "The copy folder already exists.".into(),
    )));
    state.input(Input::CopyRetained(u64::MAX), Instant::now());
    assert!(
        state.error.as_deref().unwrap().contains("already exists"),
        "a stale selection is a no-op"
    );
    state.input(Input::RetainedDatasets, Instant::now());
    drive(&mut state, |state| !state.busy());
    assert!(state.error.is_none());
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
fn data_only_setup_requires_explicit_source_add_and_provider_consent() {
    let stub = Arc::new(Stub::default());
    let mut state = configured(stub.clone());
    state.input(Input::Camera("sim:0".into(), false), Instant::now());
    state.input(
        Input::DataSourceDraft("sim-signal://line1".into()),
        Instant::now(),
    );
    assert!(!state.valid_to_start());
    assert!(state.data_sources.is_empty());
    state.input(Input::AddDataSource, Instant::now());
    assert_eq!(state.data_sources.len(), 1);
    assert!(state.start_reason().unwrap().contains("AI provider"));
    state.input(Input::AllowData(true), Instant::now());
    assert!(state.valid_to_start(), "{:?}", state.start_reason());
    state.input(Input::Start, Instant::now());
    drive(&mut state, |state| state.outcome.is_some());
    let requests = stub.requests.lock().unwrap();
    assert!(requests[0].scope.cameras.is_empty());
    assert_eq!(requests[0].scope.data_sources, vec!["sim-signal://line1"]);
    assert!(requests[0].scope.allow_data);
    drop(requests);
    state.input(Input::NewTask, Instant::now());
    assert!(state.data_sources.is_empty());
    assert!(!state.allow_data);
}

fn separate_data_setup(stub: Arc<Stub>) -> State {
    let mut state = configured(stub);
    state.context.as_mut().unwrap().task_data_available = true;
    state.input(Input::Camera("sim:0".into(), false), Instant::now());
    state.input(
        Input::DataSourceDraft("sim-signal://private-task".into()),
        Instant::now(),
    );
    state.input(Input::AddDataSource, Instant::now());
    state.input(Input::AllowData(true), Instant::now());
    state
}

#[test]
fn separate_task_dataset_is_opt_in_and_mutually_exclusive_with_current_dataset() {
    let mut state = separate_data_setup(Arc::new(Stub::default()));
    assert!(!state.separate_dataset);
    assert!(state.task_data_request().is_none());
    state.context.as_mut().unwrap().snapshot.dataset = Some(crate::dataset::DatasetStatus {
        slot_id: Some("a".repeat(32)),
        ..Default::default()
    });
    state.input(Input::UseDataset(true), Instant::now());
    assert_eq!(state.dataset, Some("a".repeat(32)));
    state.input(Input::SeparateDataset(true), Instant::now());
    assert!(state.separate_dataset);
    assert!(state.dataset.is_none());
    state.input(Input::UseDataset(true), Instant::now());
    assert!(!state.separate_dataset);
    assert_eq!(state.dataset, Some("a".repeat(32)));
    state.input(Input::SeparateDataset(true), Instant::now());
    state.input(Input::NewTask, Instant::now());
    assert!(!state.separate_dataset);
    assert!(state.task_data_request().is_none());
}

#[test]
fn separate_task_dataset_propagates_the_reviewed_cap_and_fresh_sensor_scope() {
    let stub = Arc::new(Stub::default());
    let mut state = separate_data_setup(stub.clone());
    state.input(Input::SeparateDataset(true), Instant::now());
    let expected = (state.limits.max_bytes / 2).min(state.limits.max_data_bytes);
    assert!(expected > 0);
    assert!(state.valid_to_start(), "{:?}", state.start_reason());
    state.input(Input::Start, Instant::now());
    drive(&mut state, |state| state.outcome.is_some());
    let requests = stub.requests.lock().unwrap();
    assert_eq!(requests[0].task_data.as_ref().unwrap().max_bytes, expected);
    assert!(requests[0].scope.cameras.is_empty());
    assert!(requests[0].scope.dataset.is_none());
    assert!(!requests[0].scope.effects_may_outlive_run);
    assert!(requests[0].scope.allow_data);
    assert_eq!(
        requests[0].scope.data_sources,
        ["sim-signal://private-task"]
    );
}

#[test]
fn separate_dataset_reservation_leaves_room_for_payload_and_export_budgets() {
    let mut state = separate_data_setup(Arc::new(Stub::default()));
    state.input(Input::SeparateDataset(true), Instant::now());
    for field in ["max_bytes", "max_data_bytes"] {
        state.input(
            Input::Limit {
                field: field.into(),
                value: "512 MiB".into(),
            },
            Instant::now(),
        );
    }
    assert_eq!(state.task_dataset_cap(), Some(256 * 1024 * 1024));
    assert_eq!(
        state.task_data_request().unwrap().max_bytes,
        256 * 1024 * 1024
    );
}

#[test]
fn separate_task_dataset_refuses_incompatible_or_unknown_scope_without_silent_fallback() {
    let stub = Arc::new(Stub::default());
    let mut state = separate_data_setup(stub.clone());
    state.input(Input::SeparateDataset(true), Instant::now());
    state.input(Input::Camera("sim:0".into(), true), Instant::now());
    assert!(state.start_reason().unwrap().contains("sensor-only"));
    assert!(state.separate_dataset);
    state.input(Input::Start, Instant::now());
    assert!(stub.requests.lock().unwrap().is_empty());
    state.input(Input::Camera("sim:0".into(), false), Instant::now());
    state.context.as_mut().unwrap().task_data_available = false;
    assert!(state.start_reason().unwrap().contains("unavailable"));
    assert!(state.separate_dataset);
    assert!(state.task_data_request().is_none());
    state.input(Input::Start, Instant::now());
    assert!(stub.requests.lock().unwrap().is_empty());
    state.context.as_mut().unwrap().task_data_available = true;
    state.input(Input::AllowData(false), Instant::now());
    assert!(state.start_reason().unwrap().contains("AI provider"));
    state.input(Input::AllowData(true), Instant::now());
    state.data_sources.insert("signal:0123456789abcdef".into());
    assert!(state.start_reason().unwrap().contains("new captures"));
    state.data_sources.remove("signal:0123456789abcdef");
    assert!(state.valid_to_start(), "{:?}", state.start_reason());
}

#[test]
fn separate_task_dataset_resume_does_not_adopt_or_follow_the_shared_dataset() {
    let mut state = separate_data_setup(Arc::new(Stub::default()));
    state.input(Input::SeparateDataset(true), Instant::now());
    state.snapshot.state = RunState::Paused;
    state.context.as_mut().unwrap().snapshot.dataset = Some(crate::dataset::DatasetStatus {
        slot_id: Some("b".repeat(32)),
        ..Default::default()
    });
    assert!(state.data_scope_issue().is_none());
    assert!(state.dataset.is_none());
}

fn cached_source_context() -> (Context, tools::context::DataSourceRef) {
    let mut context = fixture_context(fixture_snapshot());
    let fresh = crate::gui::source_setup::tests::bound_context(&mut context);
    let mut source = crate::gui::signals::fixture().remove(0);
    source.id = fresh.reference.clone();
    source.uri = fresh.uri.clone();
    source.protocol = crate::signals::Protocol::Mqtt;
    source.auth = fresh.auth.clone();
    context.data_sources = vec![tools::context::DataSourceRef::cached_source(&source)];
    context.snapshot.signals = vec![source];
    (context, fresh)
}

#[test]
fn cached_source_selection_pins_the_account_and_requires_reselection_after_identity_changes() {
    let (context, fresh) = cached_source_context();
    let original = fresh.auth.clone().unwrap();
    let mut rotated = original.clone();
    rotated.epoch = "d".repeat(32);
    let mut other = rotated.clone();
    other.profile.id = "e".repeat(32);
    other.profile.revision = "f".repeat(32);
    for replacement in [None, Some(rotated), Some(other)] {
        let mut state = configured(Arc::new(Stub::default()));
        state.cameras.clear();
        state.allow_data = true;
        state.set_context(context.clone(), None);
        state.input(
            Input::DataSource(fresh.reference.clone(), true),
            Instant::now(),
        );
        assert!(state.valid_to_start(), "{:?}", state.start_reason());
        let selected = state.cached_sources[&fresh.reference].clone();
        assert!(selected.uri.is_empty());
        assert_eq!(selected.auth, Some(original.clone()));
        assert!(state.task_scope().source_bindings.is_empty());
        let public = serde_json::to_string(&state.task_scope()).unwrap();
        assert!(!public.contains("cached_source_bindings"));
        assert!(!public.contains("private-topic"));
        assert!(!public.contains(&original.epoch));
        let mut changed = context.clone();
        changed.snapshot.signals[0].auth = replacement.clone();
        changed.data_sources = vec![tools::context::DataSourceRef::cached_source(
            &changed.snapshot.signals[0],
        )];
        state.set_context(changed, None);
        assert_eq!(
            state.cached_sources[&fresh.reference], selected,
            "fresh metadata cannot adopt another account or epoch"
        );
        assert!(!state.valid_to_start());
        assert!(
            state
                .start_reason()
                .unwrap()
                .contains("select the current source again")
        );
        state.input(
            Input::DataSource(fresh.reference.clone(), false),
            Instant::now(),
        );
        assert!(state.cached_sources.is_empty());
        state.input(
            Input::DataSource(fresh.reference.clone(), true),
            Instant::now(),
        );
        assert!(state.valid_to_start(), "{:?}", state.start_reason());
        assert_eq!(state.cached_sources[&fresh.reference].auth, replacement);
        assert!(state.task_scope().cached_source_bindings[0].uri.is_empty());
    }
}

#[test]
fn fresh_source_choices_and_checkbox_removal_clear_all_cached_representations() {
    let (context, fresh) = cached_source_context();
    let mut state = configured(Arc::new(Stub::default()));
    state.cameras.clear();
    state.allow_data = true;
    state.set_context(context.clone(), None);
    state.input(
        Input::DataSource(fresh.reference.clone(), true),
        Instant::now(),
    );
    state.input(Input::DataSourceDraft(fresh.uri.clone()), Instant::now());
    state.input(Input::AddDataSource, Instant::now());
    assert!(state.cached_sources.is_empty());
    assert_eq!(state.data_sources, BTreeSet::from([fresh.uri.clone()]));
    state.input(
        Input::DataSource(fresh.reference.clone(), true),
        Instant::now(),
    );
    assert_eq!(
        state.data_sources,
        BTreeSet::from([fresh.reference.clone()])
    );
    state.source_effects(vec![crate::gui::source_setup::Effect::Bound(fresh.clone())]);
    assert!(state.cached_sources.is_empty());
    assert!(state.data_sources.is_empty());
    assert_eq!(state.profile_sources, vec![fresh.clone()]);
    state.input(
        Input::DataSource(fresh.reference.clone(), false),
        Instant::now(),
    );
    assert!(state.profile_sources.is_empty());
    state.set_context(context, None);
    state.input(
        Input::DataSource(fresh.reference.clone(), true),
        Instant::now(),
    );
    state.source_effects(vec![crate::gui::source_setup::Effect::Invalidate(
        fresh.profile_id().unwrap().into(),
    )]);
    assert!(state.cached_sources.is_empty());
    assert!(state.data_sources.is_empty());
    state.input(
        Input::DataSource(fresh.reference.clone(), true),
        Instant::now(),
    );
    state.input(Input::NewTask, Instant::now());
    assert!(state.cached_sources.is_empty());
    assert!(state.data_sources.is_empty());
}

#[test]
fn selecting_a_missing_cached_source_never_creates_a_scope_grant() {
    let mut state = configured(Arc::new(Stub::default()));
    state.input(
        Input::DataSource("signal:0123456789abcdef".into(), true),
        Instant::now(),
    );
    assert!(state.data_sources.is_empty());
    assert!(state.cached_sources.is_empty());
    assert!(
        state
            .error
            .as_ref()
            .unwrap()
            .contains("available running source")
    );
}

#[test]
fn protected_task_scope_carries_the_exact_binding_and_refuses_an_expired_epoch() {
    let stub = Arc::new(Stub::default());
    let mut state = configured(stub.clone());
    state.cameras.clear();
    let mut context = fixture_context(fixture_snapshot());
    let source = crate::gui::source_setup::tests::bound_context(&mut context);
    state.source_effects(vec![crate::gui::source_setup::Effect::Bound(
        source.clone(),
    )]);
    assert!(state.context.is_none());
    assert!(state.take_context_refresh());
    assert!(
        !state.take_context_refresh(),
        "one binding requests one fresh gather"
    );
    assert!(
        !state.valid_to_start(),
        "a source receipt does not grant provider data consent"
    );
    state.input(Input::AllowData(true), Instant::now());
    assert!(
        !state.valid_to_start(),
        "a binding requires a fresh coordinator/profile snapshot"
    );
    state.set_context(context.clone(), None);
    assert!(state.valid_to_start(), "{:?}", state.start_reason());
    let scope = state.task_scope();
    assert_eq!(scope.data_sources, vec![source.reference.clone()]);
    assert_eq!(scope.source_bindings, vec![source.clone()]);
    let serialized = serde_json::to_string(&scope).unwrap();
    assert!(!serialized.contains("private-topic"));
    assert!(!serialized.contains(&source.auth.as_ref().unwrap().epoch));
    context.snapshot.signal_bindings.clear();
    state.set_context(context, None);
    assert!(!state.valid_to_start());
    assert!(state.start_reason().unwrap().contains("Use source again"));
    state.source_effects(vec![crate::gui::source_setup::Effect::Invalidate(
        source.profile_id().unwrap().into(),
    )]);
    assert!(state.profile_sources.is_empty());
    assert!(state.task_scope().data_sources.is_empty());
}

#[test]
fn a_filled_profile_draft_blocks_task_start_until_the_editor_is_closed() {
    let stub = Arc::new(Stub::default());
    let mut state = configured(stub.clone());
    assert!(state.valid_to_start(), "{:?}", state.start_reason());
    state.source_setup.fixture();
    assert_eq!(
        state.start_reason().as_deref(),
        Some(crate::gui::source_setup::UNCOMMITTED_DRAFT)
    );
    state.input(Input::Start, Instant::now());
    assert_eq!(stub.calls.load(Ordering::SeqCst), 0);
    state.input(
        Input::SourceProfile(crate::gui::source_setup::Input::Open(false)),
        Instant::now(),
    );
    assert!(state.valid_to_start(), "{:?}", state.start_reason());
    assert!(!state.source_setup.endpoint.is_empty());
    state.input(Input::Start, Instant::now());
    drive(&mut state, |state| state.outcome.is_some());
    let requests = stub.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].scope.source_bindings.is_empty());
    assert!(requests[0].scope.data_sources.is_empty());
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
fn data_source_credentials_are_refused_before_an_endpoint_is_added() {
    let mut state = configured(Arc::new(Stub::default()));
    state.input(
        Input::DataSourceDraft("mqtt://user:synthetic-private-key@broker/topic".into()),
        Instant::now(),
    );
    assert!(
        state
            .source_draft_error()
            .unwrap()
            .contains("passwords or tokens")
    );
    state.input(Input::AddDataSource, Instant::now());
    assert!(state.data_sources.is_empty());
    assert!(
        !state
            .error
            .as_ref()
            .unwrap()
            .contains("synthetic-private-key")
    );
}
#[test]
fn task_limits_parse_units_and_enforce_hard_ceilings() {
    let mut state = configured(Arc::new(Stub::default()));
    assert!(state.valid_to_start(), "{:?}", state.start_reason());
    for (field, raw) in [
        ("max_wall_ms", "1h30m"),
        ("max_bytes", "1 GiB"),
        ("approval_timeout_ms", "45s"),
        ("max_data_bytes", "64 MiB"),
        ("max_signal_ms", "10m"),
        ("max_sources", "4"),
        ("max_samples", "10000"),
    ] {
        state.input(
            Input::Limit {
                field: field.into(),
                value: raw.into(),
            },
            Instant::now(),
        );
        assert!(state.limit_error(field).is_none());
    }
    assert_eq!(state.parsed_limits().unwrap().max_wall_ms, 5_400_000);
    for (field, raw) in [
        ("max_rounds", "1001"),
        ("min_round_interval_ms", "500ms"),
        ("max_cost_usd", "inf"),
        ("max_recordings", "33"),
    ] {
        let mut one = configured(Arc::new(Stub::default()));
        one.input(
            Input::Limit {
                field: field.into(),
                value: raw.into(),
            },
            Instant::now(),
        );
        assert!(one.limit_error(field).is_some(), "{field}");
        assert!(!one.valid_to_start());
    }
}

#[test]
fn optional_data_budgets_can_be_disabled_with_zero_and_round_trip_the_editor() {
    let mut state = configured(Arc::new(Stub::default()));
    for (field, raw) in [
        ("max_sources", "0"),
        ("max_samples", "0"),
        ("max_data_bytes", "0 B"),
        ("max_signal_ms", "0s"),
    ] {
        state.input(
            Input::Limit {
                field: field.into(),
                value: raw.into(),
            },
            Instant::now(),
        );
        assert!(state.limit_error(field).is_none(), "{field}");
    }
    assert!(state.valid_to_start());
    let limits = state.parsed_limits().unwrap();
    assert_eq!(limits.max_data_bytes, 0);
    assert_eq!(limits.max_signal_ms, 0);
    state.limit_drafts = limit_fields(&limits);
    state.limits = limits;
    assert!(state.parsed_limits().is_ok());
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
fn data_takeover_waits_for_the_workers_capture_flush_and_completion() {
    let stub = Arc::new(Stub::default());
    stub.hold.store(true, Ordering::SeqCst);
    stub.flush_hold.store(true, Ordering::SeqCst);
    let mut state = configured(stub.clone());
    state.cameras.clear();
    state.data_sources.insert("sim-signal://line1".into());
    state.allow_data = true;
    assert_eq!(state.takeover_target(), TakeoverTarget::Data);
    state.input(Input::Start, Instant::now());
    drive(&mut state, |state| !state.snapshot.run_id.is_empty());
    state.input(Input::TakeOver(TakeoverTarget::Data), Instant::now());
    drive(&mut state, |_| stub.flushing.load(Ordering::SeqCst));
    assert!(
        state.active(),
        "manual handoff must wait for the source flush acknowledgement"
    );
    assert!(state.take_takeover().is_none());
    assert_eq!(state.snapshot.state, RunState::Stopping);
    stub.flush_released.store(true, Ordering::SeqCst);
    drive(&mut state, |state| state.ready_takeover.is_some());
    assert!(!state.active());
    assert_eq!(state.take_takeover(), Some(TakeoverTarget::Data));
}
#[test]
#[cfg(feature = "s3")]
fn gui_approval_binds_secret_and_discards_late_or_task_grants() {
    let stub = Arc::new(Stub::default());
    stub.approval.store(true, Ordering::SeqCst);
    let mut state = configured(stub.clone());
    state.input(Input::Start, Instant::now());
    drive(&mut state, |s| s.approval.is_some());
    let req = state.approval.clone().unwrap();
    assert!(req.needs_secret);
    state.input(
        Input::ApprovalSecret(SecretDraft::from("synthetic-private-key".to_owned())),
        Instant::now(),
    );
    state.input(
        Input::Approve {
            id: req.id,
            scope: ApprovalScope::ForRun,
        },
        Instant::now(),
    );
    assert!(state.approval.is_some());
    state.input(Input::Open(false), Instant::now());
    assert!(state.approval_secret.as_str().is_empty());
    state.input(
        Input::ApprovalSecret(SecretDraft::from("synthetic-private-key".to_owned())),
        Instant::now(),
    );
    state.input(
        Input::Approve {
            id: req.id + 1,
            scope: ApprovalScope::Once,
        },
        Instant::now(),
    );
    assert!(state.approval.is_some());
    state.input(
        Input::Approve {
            id: req.id,
            scope: ApprovalScope::Once,
        },
        Instant::now(),
    );
    drive(&mut state, |s| s.running.is_none());
    assert!(stub.observed_human.load(Ordering::SeqCst));
    assert!(stub.observed_secret.load(Ordering::SeqCst));
    assert!(state.approval_secret.as_str().is_empty());
    assert!(!format!("{:?}", state.activity).contains("synthetic-private-key"));
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
fn signal_cleanup_refuses_replacement_instances_and_accepts_helper_restart() {
    let mut ctx = fixture_context(fixture_snapshot());
    let source = crate::signals::source_id("sim-signal://line1");
    let token = "a".repeat(32);
    ctx.snapshot.signals = vec![serde_json::from_value(json!({"id":source,"capture_id":token,"protocol":"sim-signal","uri":"sim-signal://line1","interval_ms":100,"state":"running","started_ms":0,"helper_pid":7})).unwrap()];
    let record = run::RunRecord {
        signal_captures: BTreeMap::from([(source.clone(), token.clone())]),
        ..Default::default()
    };
    let left = run::Leftover {
        kind: "signal_capture".into(),
        camera: source.clone(),
        summary: "Finite capture remains".into(),
        job_id: None,
        until_ms: Some(1000),
    };
    let plan = worker::cleanup_plan(&record, &left, &ctx).unwrap();
    assert_eq!(plan.calls[0].tool, "stop_signal_capture");
    ctx.snapshot.signals[0].helper_pid = Some(42);
    ctx.snapshot.signals[0].restarts = 1;
    assert!(worker::cleanup_plan(&record, &left, &ctx).is_ok());
    ctx.snapshot.signals[0].capture_id = "b".repeat(32);
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
