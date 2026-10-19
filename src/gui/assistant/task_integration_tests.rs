//! The desktop shutdown path uses a real runtime and private journal with a
//! blocked, injected transport. It never contacts a provider or camera worker.
use super::*;
use crate::assistant::{
    ProviderId,
    agent::tests::{deps, end_reply},
    approval::{DenyApprover, RunMode},
    http::{Headers, Http, HttpRequest, HttpResponse, ProviderError, ProviderErrorKind, Transport},
    log::{self, ActionLog},
    run::{OnHumanChange, Run, RunHandle, RunRequest, RunScope},
};
use serde_json::json;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone)]
struct AttributedHost {
    snapshot: SessionSnapshot,
    actor: Option<SessionActor>,
}
impl crate::tools::context::ToolHost for AttributedHost {
    fn snapshot(&self) -> anyhow::Result<SessionSnapshot> {
        Ok(self.snapshot.clone())
    }
    fn has_session(&self) -> bool {
        true
    }
    fn actor_id(&self) -> Option<String> {
        self.actor.as_ref().map(|actor| actor.id.clone())
    }
    fn with_actor(
        &self,
        actor: SessionActor,
    ) -> anyhow::Result<Arc<dyn crate::tools::context::ToolHost>> {
        let mut host = self.clone();
        host.actor = Some(actor);
        Ok(Arc::new(host))
    }
    fn execute(&self, _: Option<&str>, _: SessionCommand) -> anyhow::Result<serde_json::Value> {
        panic!("The shutdown fixture never executes camera commands")
    }
}

struct Gate {
    entered: AtomicBool,
    released: AtomicBool,
}
impl Transport for Gate {
    fn send(&self, _: &HttpRequest<'_>) -> Result<HttpResponse, ProviderError> {
        self.entered.store(true, Ordering::SeqCst);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.released.load(Ordering::SeqCst) {
            if Instant::now() >= deadline {
                return Err(ProviderError::new(ProviderErrorKind::Timeout));
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        Ok(HttpResponse {
            status: 200,
            headers: Headers::new(),
            body: serde_json::to_vec(&end_reply(ProviderId::Anthropic)).unwrap(),
        })
    }
}

struct LiveRun {
    handle: RunHandle,
    gate: Arc<Gate>,
    directory: PathBuf,
}
impl LiveRun {
    fn new() -> Self {
        Self::with_scope(RunScope {
            cameras: vec!["sim:0".into()],
            ..Default::default()
        })
    }
    fn data() -> Self {
        Self::with_scope(RunScope {
            data_sources: vec!["sim-signal://handoff".into()],
            allow_data: true,
            ..Default::default()
        })
    }
    fn with_scope(scope: RunScope) -> Self {
        let (mut deps, _, _) = deps(ProviderId::Anthropic, Vec::new());
        let directory = deps.log_directory.clone();
        deps.host = Arc::new(AttributedHost {
            snapshot: crate::tools::tests::fixture_snapshot(),
            actor: None,
        });
        let gate = Arc::new(Gate {
            entered: AtomicBool::new(false),
            released: AtomicBool::new(false),
        });
        deps.http = Arc::new(Http::with_transport(gate.clone()));
        let handle = Run::start(
            RunRequest {
                goal: "Review the approved scope and wait for my next instruction.".into(),
                mode: RunMode::Autonomous,
                scope,
                limits: crate::assistant::Limits::default(),
                on_human_change: OnHumanChange::Pause,
                provider: None,
                model: None,
                wait_for_answer: true,
                task_data: None,
            },
            Arc::new(deps),
            Arc::new(DenyApprover::default()),
            |_| {},
        )
        .unwrap();
        wait_until(|| {
            assert!(
                handle.result().is_none(),
                "the fixture must reach injected HTTP: {:?}",
                handle.result()
            );
            gate.entered.load(Ordering::SeqCst)
        });
        assert!(
            !handle.is_finished(),
            "the stub holds an actual running request"
        );
        assert!(!handle.status().run_id.is_empty());
        Self {
            handle,
            gate,
            directory,
        }
    }
    fn records(&self) -> Vec<serde_json::Value> {
        log::read_log_in(&self.directory, &self.handle.status().run_id).unwrap()
    }
}
impl Drop for LiveRun {
    fn drop(&mut self) {
        self.handle.stop();
        self.gate.released.store(true, Ordering::SeqCst);
        wait_until(|| self.handle.result().is_some());
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

fn wait_until(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(
            Instant::now() < deadline,
            "the bounded fixture worker did not respond"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn app_with_run(run: &LiveRun, sheet_open: bool) -> Workbench {
    let mut app = Workbench::new(SessionHandle::new(), "task-quit-tests".into(), true, None);
    app.assistant.quick.synthetic = true;
    app.assistant.quick.open = sheet_open;
    app.assistant.tasks.mount_run_for_test(run.handle.clone());
    assert!(app.assistant.tasks.active());
    app
}

#[test]
fn quit_and_native_close_require_a_human_decision_and_keep_running_preserves_the_run() {
    for native_close in [false, true] {
        for sheet_open in [false, true] {
            let run = LiveRun::new();
            let mut app = app_with_run(&run, sheet_open);
            let main = app.main_window;
            let _ = app.update(if native_close {
                Message::CloseRequested(main)
            } else {
                Message::Quit
            });
            assert!(app.assistant.quit.is_some());
            assert!(
                app.closing.is_none(),
                "the closing filter must not start before confirmation"
            );
            assert_eq!(app.assistant.quick.open, sheet_open);
            assert!(!run.handle.is_finished());
            assert!(!run.records().iter().any(|record| record["kind"] == "end"));
            let _ = app.update_assistant(Input::Task(tasks::Input::KeepRunning));
            assert!(app.assistant.quit.is_none());
            assert!(app.closing.is_none());
            assert!(app.assistant.tasks.active());
            assert_eq!(app.assistant.quick.open, sheet_open);
            assert!(!run.handle.is_finished());
        }
    }
}

#[test]
fn confirmed_quit_syncs_app_quit_before_installing_the_closing_message_filter() {
    for native_close in [false, true] {
        let run = LiveRun::new();
        let mut app = app_with_run(&run, true);
        let main = app.main_window;
        let _ = app.update(if native_close {
            Message::CloseRequested(main)
        } else {
            Message::Quit
        });
        assert!(app.closing.is_none());
        // Do not poll the returned preferences task. The log must already be
        // durable when this handler returns, while provider HTTP is blocked.
        let _ = app.update_assistant(Input::Task(tasks::Input::ConfirmQuit));
        assert!(app.closing.is_some());
        assert!(app.assistant.quit.is_none());
        assert!(!app.assistant.tasks.active());
        assert!(!run.gate.released.load(Ordering::SeqCst));
        let records = run.records();
        let ends: Vec<_> = records
            .iter()
            .filter(|record| record["kind"] == "end")
            .collect();
        assert_eq!(ends.len(), 1);
        assert_eq!(ends[0]["outcome"], "app_quit");
        run.gate.released.store(true, Ordering::SeqCst);
        wait_until(|| run.handle.result().is_some());
        assert_eq!(
            run.records()
                .iter()
                .filter(|record| record["kind"] == "end")
                .count(),
            1,
            "late completion cannot append a second end"
        );
    }
}

#[test]
fn closing_settings_does_not_stop_or_guard_a_running_task() {
    let run = LiveRun::new();
    let mut app = app_with_run(&run, false);
    let settings = window::Id::unique();
    app.settings_window = Some(settings);
    app.settings_open = true;
    let _ = app.update(Message::CloseRequested(settings));
    assert!(!app.settings_open);
    assert!(app.assistant.quit.is_none());
    assert!(app.closing.is_none());
    assert!(app.assistant.tasks.active());
    assert!(!run.handle.is_finished());
}

#[test]
fn hiding_a_running_sheet_preserves_controls_and_take_over_waits_for_a_safe_stop() {
    let run = LiveRun::new();
    let mut app = app_with_run(&run, true);
    app.assistant.tasks.mode = tasks::Mode::Autonomous;
    app.assistant.tasks.cameras.insert("sim:0".into());
    app.inspector_open = false;
    app.inspector_summoned = false;
    let _ = app.update_assistant(Input::Quick(quick::Input::Open(false)));
    assert!(!app.assistant.quick.open);
    assert!(app.assistant.tasks.active());
    assert!(app.assistant_toolbar_status(Palette::of(false)).is_some());
    let _ = app.update_assistant(Input::Task(tasks::Input::TakeOver(
        tasks::TakeoverTarget::Camera("sim:0".into()),
    )));
    assert!(
        app.assistant.tasks.active(),
        "the outstanding request must stop before handoff"
    );
    assert!(!app.inspector_open);
    run.gate.released.store(true, Ordering::SeqCst);
    wait_until(|| run.handle.result().is_some());
    app.tick_assistant();
    let _ = app.take_assistant_handoff();
    assert!(!app.assistant.tasks.active());
    assert!(!app.assistant.quick.open);
    assert!(app.inspector_open);
    assert!(app.inspector_summoned);
    assert!(app.notice.is_some());
}

#[test]
fn camera_free_data_takeover_waits_for_runtime_completion_then_opens_actual_manual_controls() {
    use iced::advanced::{Layout, layout, widget::Tree};
    let run = LiveRun::data();
    let mut app = app_with_run(&run, true);
    assert!(app.snapshot.cameras.is_empty());
    assert!(app.snapshot.connected.is_none());
    app.assistant.tasks.cameras.clear();
    app.inspector_open = false;
    app.inspector_peek = false;
    app.tab = super::super::Tab::Features;
    let _ = app.update_assistant(Input::Task(tasks::Input::TakeOver(
        tasks::TakeoverTarget::Data,
    )));
    assert!(app.assistant.tasks.active());
    assert!(app.assistant.quick.open);
    assert!(!app.inspector_open);
    assert!(!run.records().iter().any(|record| record["kind"] == "end"));
    let _ = app.take_assistant_handoff();
    assert!(
        !app.inspector_open,
        "stopping alone cannot reveal manual controls before the run ends"
    );
    run.gate.released.store(true, Ordering::SeqCst);
    wait_until(|| run.handle.result().is_some());
    app.tick_assistant();
    let _ = app.take_assistant_handoff();
    assert!(!app.assistant.tasks.active());
    assert!(!app.assistant.quick.open);
    assert_eq!(app.tab, super::super::Tab::Capture);
    assert!(app.inspector_shown());
    assert!(run.records().iter().any(|record| record["kind"] == "end"));
    assert!(app.signals.input.is_empty());
    assert!(app.handle.snapshot().signals.is_empty());
    assert!(app.handle.snapshot().signal_bindings.is_empty());
    let renderer = destinations::layout_tests::renderer();
    let mut view = app.inspector(Palette::of(false));
    let mut tree = Tree::new(view.as_widget());
    let node = view.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, Size::new(INSPECTOR, 620.0)),
    );
    let (snapshot, _, _) = accessibility::snapshot_widget(
        view.as_widget_mut(),
        &mut tree,
        Layout::new(&node),
        &renderer,
    );
    assert!(
        snapshot
            .nodes
            .iter()
            .any(|(_, node)| node.role() == accessibility::Role::TextInput
                && node.label() == Some("Add data source")
                && !node.is_disabled())
    );
    assert!(
        snapshot
            .nodes
            .iter()
            .any(|(_, node)| node.label() == Some("Protected source profiles"))
    );
}

struct JournalServices {
    directory: PathBuf,
    run_attempts: std::sync::atomic::AtomicUsize,
}
impl tasks_worker::Services for JournalServices {
    fn gather(&self) -> anyhow::Result<crate::tools::context::Context> {
        Ok(task_scenes::context(SessionSnapshot::default()))
    }
    fn run(
        &self,
        _: crate::assistant::Settings,
        _: RunRequest,
        _: &crate::assistant::run::RunControl,
        _: Arc<tasks_worker::GuiApprover>,
        _: &mut dyn FnMut(crate::assistant::run::RunEvent),
    ) -> anyhow::Result<crate::assistant::run::RunOutcome> {
        self.run_attempts.fetch_add(1, Ordering::SeqCst);
        anyhow::bail!("Crash review never starts or resumes a task.")
    }
    fn apply(
        &self,
        _: &[(crate::tools::StepId, crate::tools::ToolCall)],
        _: &[crate::tools::Checked],
        _: &tasks_worker::RecoveryGuard,
        _: &AtomicBool,
    ) -> Result<serde_json::Value, crate::tools::ApplyError> {
        panic!("Crash review cannot execute camera tools")
    }
    fn runs(&self) -> anyhow::Result<Vec<log::RunSummary>> {
        log::runs_in(&self.directory)
    }
    fn read_log(&self, id: &str) -> anyhow::Result<Vec<serde_json::Value>> {
        log::read_log_in(&self.directory, id)
    }
}

fn finish_review_job(state: &mut tasks::State) {
    wait_until(|| {
        state.tick(Instant::now());
        !state.busy()
    });
}

#[test]
fn unfinished_private_logs_are_shown_once_on_open_and_never_auto_resumed() {
    let directory = crate::assistant::agent::tests::directory();
    let services = Arc::new(JournalServices {
        directory: directory.clone(),
        run_attempts: std::sync::atomic::AtomicUsize::new(0),
    });
    let mut log = ActionLog::create(&directory, crate::tools::tests::NOW).unwrap();
    let id = log.id().to_owned();
    log.record(
        "start",
        json!({
            "goal":"Inspect a camera", "mode":"autonomous", "provider":"anthropic",
            "model":"scene-model", "scope":{"cameras":["sim:0"]},
        }),
        crate::tools::tests::NOW,
    )
    .unwrap();
    log.record(
        "observation",
        json!({"camera":"sim:0","metrics":{"brightness":0.46}}),
        crate::tools::tests::NOW + 1000,
    )
    .unwrap();
    let mut state = tasks::State::with_services(services.clone());
    state.input(tasks::Input::Open(true), Instant::now());
    finish_review_job(&mut state);
    assert!(state.crash.is_none(), "a locked live run is not a crash");
    drop(log); // Simulate process loss: the private file has no end record.
    state.input(tasks::Input::Open(true), Instant::now());
    finish_review_job(&mut state);
    let crash = state
        .crash
        .as_ref()
        .expect("the next open identifies the unfinished run");
    assert_eq!(crash.run_id, id);
    assert_eq!(crash.time_ms, crate::tools::tests::NOW + 1000);
    assert!(!state.active());
    assert_eq!(state.mode, tasks::Mode::Plan);
    assert!(state.snapshot.run_id.is_empty());
    assert_eq!(services.run_attempts.load(Ordering::SeqCst), 0);
    state.input(tasks::Input::ViewLog, Instant::now());
    finish_review_job(&mut state);
    let text = state
        .log_text
        .as_ref()
        .expect("View log reads the private journal");
    assert!(text.contains(&id));
    assert!(!text.contains("\"kind\":\"end\""));
    state.input(tasks::Input::NewTask, Instant::now());
    assert!(state.crash.is_none());
    state.input(tasks::Input::Open(true), Instant::now());
    finish_review_job(&mut state);
    assert!(
        state.crash.is_none(),
        "one window shows an unfinished run only once"
    );
    assert_eq!(services.run_attempts.load(Ordering::SeqCst), 0);
    assert_eq!(log::runs_in(&directory).unwrap().len(), 1);
    std::fs::remove_dir_all(directory).unwrap();
}
