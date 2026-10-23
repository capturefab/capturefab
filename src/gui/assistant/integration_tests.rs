use super::*;
use crate::session::{ActivityState, ActorKind, SessionActivity};

fn fixture(word: &str) -> Workbench {
    let mut app = Workbench::new(
        SessionHandle::new(),
        "quick-command-tests".into(),
        true,
        None,
    );
    assert!(app.scene_assistant_quick(word, false));
    app
}

#[test]
fn closing_the_sheet_by_each_window_action_preserves_the_turn_and_plan() {
    for action in [Action::Overview, Action::QuickCommand, Action::CloseWindow] {
        let mut app = fixture("quick-command-thinking");
        app.assistant
            .quick
            .plan
            .propose(
                crate::tools::ToolCall {
                    tool: "capture_photos".into(),
                    args: serde_json::json!({"camera":"sim:0"}),
                },
                None,
            )
            .unwrap();
        let plan = app.assistant.quick.plan.document();
        let generation = app.assistant.quick.generation;
        let _ = app.dispatch_action(action);
        assert!(!app.assistant.quick.open, "{action:?}");
        assert!(app.assistant.quick.thinking(), "{action:?}");
        assert_eq!(app.assistant.quick.generation, generation);
        assert_eq!(app.assistant.quick.plan.document(), plan);
        assert!(app.closing.is_none());
    }
}

#[test]
fn quick_command_respects_the_other_sheet_and_hands_off_without_overlap() {
    let mut app = fixture("quick-command");
    let _ = app.update_assistant(Input::Quick(quick::Input::Open(false)));
    app.help_open = true;
    let _ = app.dispatch_action(Action::QuickCommand);
    assert!(!app.assistant.quick.open);
    app.help_open = false;
    let _ = app.dispatch_action(Action::QuickCommand);
    assert!(app.assistant.quick.open);
    let _ = app.update_assistant(Input::Quick(quick::Input::Settings));
    assert!(!app.assistant.quick.open);
    assert!(!app.settings_open);
    assert_eq!(app.assistant.handoff, Some(true));
}

#[test]
fn successful_provider_setup_closes_settings_before_opening_quick_command() {
    use iced::futures::StreamExt;
    let mut app = fixture("quick-command");
    app.assistant.quick.open = false;
    assert!(app.scene_assistant_settings("settings-assistant-success", false));
    let task = app.update_assistant(Input::Settings(settings::Input::ContinueToQuick));
    assert!(!app.settings_open && !app.assistant.quick.open);
    let mut stream = iced_runtime::task::into_stream(task).unwrap();
    let output = iced::futures::executor::block_on(stream.next()).unwrap();
    let iced_runtime::Action::Output(message) = output else {
        panic!("the Settings sheet closes before the Quick Command message");
    };
    let _ = app.handle_message(message);
    assert!(app.assistant.quick.open && !app.settings_open);

    app.assistant.quick.open = false;
    app.scene_assistant_settings("settings-assistant-error", false);
    let _ = app.update_assistant(Input::Settings(settings::Input::ContinueToQuick));
    assert!(app.settings_open && !app.assistant.quick.open);
}

#[test]
fn local_assistant_activity_keeps_its_agent_label_without_participant_notices() {
    let mut app = fixture("quick-command");
    let actor = SessionActor::new("assistant:desktop:test", "Assistant", ActorKind::Agent).unwrap();
    app.assistant.local_actors.insert(actor.id.clone());
    let mut entry = SessionActivity {
        id: 1,
        origin: ActivityOrigin::Gui,
        actor,
        operation: "capture".into(),
        camera: Some("sim:0".into()),
        feature: None,
        label: "Capture a photo".into(),
        state: ActivityState::Succeeded,
        submitted_at_ms: 1,
        started_at_ms: Some(1),
        finished_at_ms: Some(2),
        detail: Some("photo.png".into()),
        file_count: 1,
        files: vec!["photo.png".into()],
        job: None,
    };
    assert!(!app.other_activity(&entry));
    assert!(app.activity_texts(&entry).0.contains("AI agent"));
    app.snapshot.activity.push(entry.clone());
    app.sync_client_activity();
    assert!(app.notice.is_none());
    entry.actor.id = "another-agent".into();
    assert!(app.other_activity(&entry));
}

#[test]
fn quick_command_chords_work_in_editors_and_preserve_non_mac_control_k() {
    for os in [Os::Mac, Os::Windows, Os::Nix] {
        let chord = Action::QuickCommand.bindings(os)[0];
        assert_eq!(Action::find(&chord, false, os), Some(Action::QuickCommand));
        if os != Os::Mac {
            let mut plain = chord;
            plain.mods.shift = false;
            assert_eq!(Action::find(&plain, false, os), None);
        }
    }
}
