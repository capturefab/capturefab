use super::*;
use std::sync::{
    Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

struct Fake {
    calls: AtomicUsize,
    saves: AtomicUsize,
    binds: AtomicUsize,
    password_received: AtomicBool,
    fail_save: AtomicBool,
    block_bind: AtomicBool,
    bind_entered: AtomicBool,
    bind_release: AtomicBool,
    draft: Mutex<Option<ProfileDraft>>,
    expected: Mutex<Vec<Option<ProfileRef>>>,
    listed: Mutex<Vec<ProfileInfo>>,
}
impl Default for Fake {
    fn default() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            saves: AtomicUsize::new(0),
            binds: AtomicUsize::new(0),
            password_received: AtomicBool::new(false),
            fail_save: AtomicBool::new(false),
            block_bind: AtomicBool::new(false),
            bind_entered: AtomicBool::new(false),
            bind_release: AtomicBool::new(false),
            draft: Mutex::new(None),
            expected: Mutex::new(vec![]),
            listed: Mutex::new(vec![]),
        }
    }
}
fn info() -> ProfileInfo {
    ProfileInfo {
        profile: ProfileRef {
            id: "a".repeat(32),
            revision: "b".repeat(32),
        },
        name: "line1".into(),
        endpoint: "mqtts://broker.example/private-topic".into(),
        reference: crate::signals::source_id("mqtts://broker.example/private-topic"),
        protocol: crate::signals::Protocol::Mqtt,
        secret: SecretKind::Keychain,
        insecure: false,
    }
}
fn binding() -> DataSourceRef {
    let profile = info();
    DataSourceRef::from_binding(&crate::signals::auth::BindingReceipt {
        endpoint: profile.endpoint,
        reference: profile.reference,
        auth: crate::signals::auth::AuthBinding {
            profile: profile.profile,
            epoch: "c".repeat(32),
        },
    })
    .unwrap()
}
pub(in crate::gui) fn bound_context(context: &mut crate::tools::context::Context) -> DataSourceRef {
    let source = binding();
    context.data_profiles = vec![info()];
    context.snapshot.signal_bindings = vec![crate::signals::auth::BindingReceipt {
        endpoint: source.uri.clone(),
        reference: source.reference.clone(),
        auth: source.auth.clone().unwrap(),
    }];
    source
}
impl Fake {
    fn entered(&self) {
        assert_eq!(
            std::thread::current().name(),
            Some("capturefab-source-profiles")
        );
        self.calls.fetch_add(1, Ordering::SeqCst);
    }
}
impl Services for Fake {
    fn list(&self) -> Result<Vec<ProfileInfo>, String> {
        self.entered();
        Ok(self.listed.lock().unwrap().clone())
    }
    fn edit(&self, _: &ProfileRef) -> Result<ProfileDraft, String> {
        self.entered();
        self.draft
            .lock()
            .unwrap()
            .clone()
            .ok_or("No saved draft.".into())
    }
    fn save(
        &self,
        expected: Option<&ProfileRef>,
        draft: ProfileDraft,
        password: Option<SecretDraft>,
    ) -> Result<ProfileInfo, String> {
        self.entered();
        self.saves.fetch_add(1, Ordering::SeqCst);
        self.expected.lock().unwrap().push(expected.cloned());
        self.password_received.store(
            password
                .as_ref()
                .is_some_and(|password| password.as_str() == "synthetic-password-never-reflected"),
            Ordering::SeqCst,
        );
        *self.draft.lock().unwrap() = Some(draft);
        if self.fail_save.load(Ordering::SeqCst) {
            return Err(format!(
                "Synthetic store failed: {}",
                password.as_ref().map_or("", SecretDraft::as_str)
            ));
        }
        Ok(info())
    }
    fn bind(&self, _: &ProfileRef) -> Result<DataSourceRef, String> {
        self.entered();
        self.binds.fetch_add(1, Ordering::SeqCst);
        self.bind_entered.store(true, Ordering::SeqCst);
        let until = Instant::now() + Duration::from_secs(3);
        while self.block_bind.load(Ordering::SeqCst) && !self.bind_release.load(Ordering::SeqCst) {
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(1));
        }
        Ok(binding())
    }
    fn delete(&self, _: &ProfileRef) -> Result<(), String> {
        self.entered();
        Ok(())
    }
}
fn wait(state: &mut State) -> Vec<Effect> {
    let until = Instant::now() + Duration::from_secs(4);
    let mut effects = vec![];
    while state.pending {
        effects.extend(state.tick());
        assert!(
            Instant::now() < until,
            "source profile worker did not settle"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    effects
}
fn fill(state: &mut State) {
    for input in [
        Input::Name("line1".into()),
        Input::Endpoint(info().endpoint),
        Input::Username("local-reader-private-name".into()),
        Input::Authentication(SecretKind::Keychain),
        Input::Password("synthetic-password-never-reflected".to_string().into()),
    ] {
        state.input(input, true);
    }
}

#[test]
fn construction_and_views_do_no_profile_or_credential_io() {
    let services = Arc::new(Fake::default());
    let state = State::with_services(services.clone());
    let _view = view(
        &state,
        Palette::of(false),
        true,
        "source-test",
        Purpose::Approve,
    );
    assert_eq!(services.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn a_disconnected_profile_worker_can_be_reopened_or_refreshed_without_adopting_stale_bindings() {
    for reopen in [false, true] {
        let services = Arc::new(Fake::default());
        *services.listed.lock().unwrap() = vec![info()];
        let mut state = State::with_services(services.clone());
        state.input(Input::Open(true), true);
        wait(&mut state);
        state.input(Input::Select(info().profile), true);
        assert!(state.loaded);

        // Simulate a worker that queues a binding then terminates before the
        // UI polls. Its late binding must not survive the disconnect fence.
        let (send, requests) = std::sync::mpsc::channel();
        drop(requests);
        let (replies, receive) = std::sync::mpsc::channel();
        let generation = state.generation.load(Ordering::SeqCst);
        assert!(
            replies
                .send((generation, worker::Reply::Bound(Ok(binding()))))
                .is_ok()
        );
        drop(replies);
        state.worker = Some(worker::Worker { send, receive });
        state.pending = true;
        let effects = state.tick();
        assert!(
            effects.is_empty(),
            "a disconnected worker cannot add source scope"
        );
        assert!(state.worker.is_none());
        assert!(!state.loaded && !state.pending);
        assert!(state.generation.load(Ordering::SeqCst) > generation);
        assert!(
            state
                .error
                .as_deref()
                .unwrap()
                .contains("Reopen or refresh")
        );

        let mut replacement = info();
        replacement.profile.revision = "d".repeat(32);
        *services.listed.lock().unwrap() = vec![replacement.clone()];
        if reopen {
            state.input(Input::Open(false), true);
            state.input(Input::Open(true), true);
        } else {
            state.input(Input::Reload, true);
        }
        let effects = wait(&mut state);
        assert!(state.loaded && state.worker.is_some());
        assert!(state.error.is_none());
        assert_eq!(state.profiles[0].profile, replacement.profile);
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, Effect::Bound(_))),
            "refreshing metadata never adopts a stale binding"
        );
        assert_eq!(services.calls.load(Ordering::SeqCst), 2);
        assert_eq!(services.binds.load(Ordering::SeqCst), 0);
        assert!(state.tick().is_empty());
        assert!(
            state.error.is_none(),
            "the dead channel must no longer be polled"
        );
    }
}

#[test]
fn a_failed_profile_worker_send_can_be_retried_with_a_replacement_worker() {
    let services = Arc::new(Fake::default());
    let mut state = State::with_services(services.clone());
    let (send, requests) = std::sync::mpsc::channel();
    drop(requests);
    let (replies, receive) = std::sync::mpsc::channel();
    let generation = state.generation.load(Ordering::SeqCst);
    state.worker = Some(worker::Worker { send, receive });
    state.loaded = true;
    state.input(Input::Reload, true);
    assert!(state.worker.is_none());
    assert!(!state.loaded && !state.pending);
    assert!(state.error.is_some());
    assert!(
        replies
            .send((generation, worker::Reply::Bound(Ok(binding()))))
            .is_err(),
        "the failed worker's reply channel is retired too"
    );
    state.input(Input::Reload, true);
    assert!(wait(&mut state).is_empty());
    assert!(state.loaded && state.worker.is_some());
    assert!(state.error.is_none());
    assert_eq!(services.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn saving_moves_the_secret_to_worker_and_use_returns_only_an_opaque_binding() {
    let services = Arc::new(Fake::default());
    let mut state = State::with_services(services.clone());
    state.input(Input::Open(true), true);
    wait(&mut state);
    fill(&mut state);
    assert!(state.has_uncommitted_draft());
    assert!(!format!("{state:?}").contains("local-reader-private-name"));
    state.input(Input::Save, true);
    assert!(state.password.as_str().is_empty());
    let effects = wait(&mut state);
    assert!(
        !state.has_uncommitted_draft(),
        "a saved profile still needs a separate Use action"
    );
    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, Effect::Bound(_)))
    );
    assert!(services.password_received.load(Ordering::SeqCst));
    assert_eq!(services.binds.load(Ordering::SeqCst), 0);
    let metadata = serde_json::to_string(&state.profiles).unwrap();
    assert!(!metadata.contains("local-reader-private-name"));
    assert!(!metadata.contains("synthetic-password"));
    state.input(Input::Use, true);
    let effects = wait(&mut state);
    let bound = effects
        .into_iter()
        .find_map(|effect| match effect {
            Effect::Bound(binding) => Some(binding),
            _ => None,
        })
        .unwrap();
    assert_eq!(bound, binding());
    assert!(!bound.uri.contains('@'));
    assert!(!format!("{bound:?}").contains("private-topic"));
    let effects = state.input(Input::Edit, true);
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect,Effect::Invalidate(id) if id==&info().profile.id))
    );
    wait(&mut state);
    assert_eq!(state.username, "local-reader-private-name");
    assert!(state.password.as_str().is_empty());
    state.input(Input::Save, true);
    wait(&mut state);
    assert_eq!(
        *services.expected.lock().unwrap(),
        vec![None, Some(info().profile)]
    );
}

#[test]
fn refreshed_profile_metadata_revokes_removed_revisions_without_loading_credentials() {
    let services = Arc::new(Fake::default());
    let mut state = State::with_services(services.clone());
    state.profiles = vec![info()];
    state.selected = Some(info().profile);
    state.loaded = true;
    state.input(Input::Reload, true);
    let effects = wait(&mut state);
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect,Effect::Invalidate(id) if id==&info().profile.id))
    );
    assert!(state.selected.is_none());
    assert!(state.profiles.is_empty());
    assert_eq!(services.binds.load(Ordering::SeqCst), 0);
    assert_eq!(services.saves.load(Ordering::SeqCst), 0);
}

#[test]
fn refreshing_metadata_preserves_the_active_draft_and_its_scope_guard() {
    let services = Arc::new(Fake::default());
    *services.listed.lock().unwrap() = vec![info()];
    let mut state = State::with_services(services.clone());
    state.open = true;
    state.editing = true;
    state.loaded = true;
    fill(&mut state);
    state.input(Input::Reload, true);
    wait(&mut state);
    assert!(state.editing);
    assert!(state.has_uncommitted_draft());
    assert_eq!(state.username, "local-reader-private-name");
    assert_eq!(
        state.password.as_str(),
        "synthetic-password-never-reflected"
    );
    assert!(
        state.selected.is_none(),
        "a new draft never adopts a listed profile"
    );
}

#[test]
fn a_metadata_refresh_cannot_drop_an_editing_profiles_compare_and_swap_revision() {
    let services = Arc::new(Fake::default());
    let mut replacement = info();
    replacement.profile.revision = "d".repeat(32);
    *services.listed.lock().unwrap() = vec![replacement];
    let mut state = State::with_services(services);
    state.open = true;
    state.editing = true;
    state.loaded = true;
    state.profiles = vec![info()];
    state.selected = Some(info().profile);
    fill(&mut state);
    state.input(Input::Reload, true);
    let effects = wait(&mut state);
    assert_eq!(
        state.selected,
        Some(info().profile),
        "saving must still compare the originally edited revision"
    );
    assert!(state.has_uncommitted_draft());
    assert!(state.password.as_str().is_empty());
    assert!(
        state
            .error
            .as_ref()
            .unwrap()
            .contains("latest saved version")
    );
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect,Effect::Invalidate(id) if id==&info().profile.id))
    );
}

#[test]
fn deleting_the_last_profile_exposes_a_native_new_source_action() {
    use iced::advanced::{Layout, layout, widget::Tree};
    let mut state = State::with_services(Arc::new(Fake::default()));
    state.open = true;
    state.loaded = true;
    state.profiles = vec![info()];
    state.selected = Some(info().profile);
    state.input(Input::Delete, true);
    wait(&mut state);
    assert!(state.profiles.is_empty());
    assert!(!state.editing);
    let renderer = crate::gui::destinations::layout_tests::renderer();
    let mut view = accessibility::mapped(
        super::view(
            &state,
            Palette::of(false),
            true,
            "source-delete-test",
            Purpose::Log,
        ),
        |input| Message::Signals(crate::gui::signals::Msg::Profile(input)),
    );
    let mut tree = Tree::new(view.as_widget());
    let node = view.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, Size::new(300.0, 620.0)),
    );
    let (snapshot, _, _) = accessibility::snapshot_widget(
        view.as_widget_mut(),
        &mut tree,
        Layout::new(&node),
        &renderer,
    );
    assert!(snapshot.nodes.iter().any(|(_, node)| node.label()
        == Some("Create protected source profile")
        && node.role() == accessibility::Role::Button
        && !node.is_disabled()));
    drop(view);
    state.input(Input::New, true);
    assert!(state.editing);
    assert!(state.name.is_empty());
}

#[test]
fn failed_store_does_not_bind_or_reflect_the_submitted_password() {
    let services = Arc::new(Fake::default());
    services.fail_save.store(true, Ordering::SeqCst);
    let mut state = State::with_services(services.clone());
    fill(&mut state);
    state.input(Input::Save, true);
    wait(&mut state);
    assert!(!state.error.as_ref().unwrap().contains("synthetic-password"));
    assert_eq!(services.binds.load(Ordering::SeqCst), 0);
    assert!(state.selected.is_none());
    assert!(state.password.as_str().is_empty());
}

#[test]
fn closing_during_bind_discards_its_late_scope_receipt() {
    let services = Arc::new(Fake::default());
    services.block_bind.store(true, Ordering::SeqCst);
    let mut state = State::with_services(services.clone());
    state.profiles = vec![info()];
    state.selected = Some(info().profile);
    state.loaded = true;
    state.input(Input::Use, true);
    let until = Instant::now() + Duration::from_secs(3);
    while !services.bind_entered.load(Ordering::SeqCst) {
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(1));
    }
    state.input(Input::Open(false), true);
    services.bind_release.store(true, Ordering::SeqCst);
    assert!(wait(&mut state).is_empty());
}

#[test]
fn credential_inputs_and_disabled_edits_never_leak_or_change_scope() {
    let mut state = State::default();
    fill(&mut state);
    for input in [
        Input::Username("private-user".into()),
        Input::Environment("CAPTUREFAB_SIGNAL_PRIVATE_NAME".into()),
        Input::Password("synthetic-password-never-reflected".to_string().into()),
    ] {
        assert!(!format!("{input:?}").contains("private"));
        state.input(input, false);
    }
    assert_eq!(state.username, "local-reader-private-name");
    assert_eq!(state.authentication, SecretKind::Keychain);
    state.input(Input::Open(false), true);
    assert!(state.password.as_str().is_empty());
    state.reset();
    assert!(state.username.is_empty());
    assert!(state.environment.is_empty());
    assert!(state.selected.is_none());
}

#[test]
fn a_filled_editor_blocks_scope_actions_only_while_it_is_open() {
    let mut state = State::default();
    state.fixture();
    assert!(state.has_uncommitted_draft());
    state.input(Input::Open(false), true);
    assert!(!state.has_uncommitted_draft());
    assert_eq!(state.username, "line1-reader");
    assert!(!state.endpoint.is_empty());
    assert!(state.password.as_str().is_empty());
    state.input(Input::Open(true), true);
    assert!(state.has_uncommitted_draft());
    state.editing = false;
    assert!(!state.has_uncommitted_draft());
}
