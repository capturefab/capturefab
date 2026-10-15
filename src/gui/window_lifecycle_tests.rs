//! Completions and native events can arrive after Settings has been removed
//! from the model but before its asynchronous native close has completed.
use super::*;
use iced::futures::StreamExt;
use iced_runtime::{Action as RuntimeAction, window::Action as WindowAction};

#[derive(Debug, PartialEq)]
enum Effect {
    Run(window::Id),
    Close(window::Id),
    Show(window::Id),
    Focus(window::Id),
    Scale(window::Id),
    Mode(window::Id),
    Exit,
    Output,
    Other,
}

fn effects(task: Task<Message>) -> Vec<Effect> {
    effects_with_mode(task, None)
}

fn effects_with_mode(task: Task<Message>, mode: Option<window::Mode>) -> Vec<Effect> {
    let Some(mut stream) = iced_runtime::task::into_stream(task) else {
        return Vec::new();
    };
    iced::futures::executor::block_on(async {
        let mut effects = Vec::new();
        while let Some(action) = stream.next().await {
            effects.push(match action {
                RuntimeAction::Window(WindowAction::Run(id, _)) => Effect::Run(id),
                RuntimeAction::Window(WindowAction::Close(id)) => Effect::Close(id),
                RuntimeAction::Window(WindowAction::SetMode(id, window::Mode::Windowed)) => {
                    Effect::Show(id)
                }
                RuntimeAction::Window(WindowAction::GainFocus(id)) => Effect::Focus(id),
                RuntimeAction::Window(WindowAction::GetScaleFactor(id, _)) => Effect::Scale(id),
                RuntimeAction::Window(WindowAction::GetMode(id, reply)) => {
                    if let Some(mode) = mode {
                        reply.send(mode).unwrap();
                    }
                    Effect::Mode(id)
                }
                RuntimeAction::Exit => Effect::Exit,
                RuntimeAction::Output(_) => Effect::Output,
                _ => Effect::Other,
            });
            // Unanswered native queries are canceled, like iced does when the
            // requested window has disappeared. Never create a native adapter.
        }
        effects
    })
}

fn mode_result(task: Task<Message>, mode: window::Mode) -> Message {
    let mut stream = iced_runtime::task::into_stream(task).expect("alert queries its native mode");
    iced::futures::executor::block_on(async {
        while let Some(action) = stream.next().await {
            match action {
                RuntimeAction::Window(WindowAction::GetMode(_, reply)) => reply.send(mode).unwrap(),
                RuntimeAction::Output(message) => return message,
                _ => panic!("mode query cannot show or focus a window"),
            }
        }
        panic!("native mode must return through the model");
    })
}

#[cfg(target_os = "macos")]
fn recovery_result(task: Task<Message>, before_callback: impl FnOnce()) -> Message {
    struct UnavailableWindow;
    impl raw_window_handle::HasWindowHandle for UnavailableWindow {
        fn window_handle(
            &self,
        ) -> Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError> {
            Err(raw_window_handle::HandleError::Unavailable)
        }
    }
    impl raw_window_handle::HasDisplayHandle for UnavailableWindow {
        fn display_handle(
            &self,
        ) -> Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError> {
            Err(raw_window_handle::HandleError::Unavailable)
        }
    }
    let mut before_callback = Some(before_callback);
    let mut stream = iced_runtime::task::into_stream(task).unwrap();
    iced::futures::executor::block_on(async {
        while let Some(action) = stream.next().await {
            match action {
                RuntimeAction::Window(WindowAction::Run(_, callback)) => {
                    before_callback.take().unwrap()();
                    // Exercise the actual callback's async return without
                    // installing an adapter or touching any OS state.
                    callback(&UnavailableWindow);
                }
                RuntimeAction::Output(message) => return message,
                _ => {}
            }
        }
        panic!("recovery discarded its native attachment completion");
    })
}

fn app() -> Workbench {
    let mut app = Workbench::new(SessionHandle::new(), "window-lifecycle".into(), false, None);
    app.address = "unfinished camera draft".into();
    app.window_editor_focus.insert(app.main_window, true);
    app.menu_editor = true;
    app
}

#[cfg(target_os = "macos")]
fn close_settings() -> (Workbench, window::Id) {
    let mut app = app();
    let settings = window::Id::unique();
    app.settings_window = Some(settings);
    app.settings_open = true;
    app.window_scales.insert(settings, 2.0);
    app.window_editor_focus.insert(settings, false);
    drop(app.update(Message::WindowFocused(settings)));
    let main = app.main_window;
    assert_eq!(
        effects(app.update(Message::Settings(false))),
        vec![
            Effect::Run(settings),
            Effect::Close(settings),
            Effect::Focus(main)
        ],
    );
    assert_eq!(app.settings_window, None);
    assert!(!app.settings_open);
    assert!(!app.window_scales.contains_key(&settings));
    assert!(!app.window_editor_focus.contains_key(&settings));
    drop(app.update(Message::WindowFocused(main)));
    (app, settings)
}

fn main_context_is_intact(app: &Workbench) {
    assert_eq!(app.focused_window, app.main_window);
    assert_eq!(app.key_window, Some(app.main_window));
    assert!(app.menu_editor);
    assert_eq!(app.window_editor_focus.get(&app.main_window), Some(&true));
    assert_eq!(app.address, "unfinished camera draft");
    assert!(app.closing.is_none());
    let edits: Vec<_> = app
        .command_menus()
        .into_iter()
        .flat_map(|menu| menu.items.into_iter().flatten())
        .filter(|item| matches!(item.command, menus::Command::Edit(_)))
        .collect();
    assert_eq!(edits.len(), 4);
    assert!(edits.iter().all(|item| item.enabled));
}

#[test]
fn live_window_open_activation_and_main_close_keep_their_normal_tasks() {
    let mut app = app();
    let main = app.main_window;
    let settings = window::Id::unique();
    app.settings_window = Some(settings);
    app.settings_open = true;
    for id in [main, settings] {
        assert_eq!(
            effects(app.update(Message::WindowOpened(id))),
            vec![Effect::Run(id), Effect::Scale(id)],
        );
        let activated = effects(app.update(Message::AccessibilityAttached(id, true)));
        assert!(activated.contains(&Effect::Show(id)), "{activated:?}");
        assert_eq!(activated.last(), Some(&Effect::Focus(id)));
    }
    drop(app.update(Message::WindowFocused(settings)));
    drop(app.update(Message::WindowUnfocused(main)));
    assert_eq!(app.key_window, Some(settings));
    assert_eq!(app.focused_window, settings);
    assert!(!app.menu_editor);
    assert_eq!(
        effects(app.update(Message::WindowScale(settings, 2.0))),
        vec![],
    );
    assert_eq!(app.window_scales.get(&settings), Some(&2.0));
    drop(app.update(Message::WindowFocused(main)));
    assert!(app.menu_editor);
    // No saved preferences means the ordinary close path immediately delivers
    // its flush completion, then detaches both live adapters before exit.
    let mut stream = iced_runtime::task::into_stream(app.update(Message::CloseRequested(main)))
        .expect("close produces a flush completion");
    let Some(RuntimeAction::Output(message @ Message::PrefsFlushed(Ok(())))) =
        iced::futures::executor::block_on(stream.next())
    else {
        panic!("main close must retain its preference flush");
    };
    let closed = effects(app.update(message));
    assert!(closed.contains(&Effect::Run(main)));
    assert!(closed.contains(&Effect::Run(settings)));
    assert_eq!(closed.last(), Some(&Effect::Exit));
}

#[test]
#[cfg(target_os = "macos")]
fn a_new_settings_window_still_accepts_delayed_initial_activation() {
    let (mut app, retired) = close_settings();
    let new = window::Id::unique();
    app.settings_window = Some(new);
    app.settings_open = true;
    app.focused_window = new;
    assert_eq!(
        effects(app.update(Message::WindowOpened(new))),
        vec![Effect::Run(new), Effect::Scale(new)],
    );
    let activated = effects(app.update(Message::AccessibilityAttached(new, true)));
    assert!(activated.contains(&Effect::Show(new)));
    assert_eq!(activated.last(), Some(&Effect::Focus(new)));
    drop(app.update(Message::WindowFocused(new)));
    assert!(effects(app.update(Message::WindowFocused(retired))).is_empty());
    assert_eq!(app.key_window, Some(new));
    assert_eq!(app.focused_window, new);
    let closed = effects(app.update(Message::CloseRequested(new)));
    assert_eq!(
        closed,
        vec![
            Effect::Run(new),
            Effect::Close(new),
            Effect::Focus(app.main_window)
        ]
    );
    assert!(app.closing.is_none());
    assert_eq!(app.settings_window, None);
}

#[test]
#[cfg(target_os = "macos")]
fn retired_settings_focus_cannot_replace_the_restored_editor_context() {
    let (mut app, settings) = close_settings();
    assert!(effects(app.update(Message::WindowFocused(settings))).is_empty());
    main_context_is_intact(&app);
}

#[test]
#[cfg(target_os = "macos")]
fn retired_settings_open_completions_cannot_show_or_activate_it() {
    for attached in [None, Some(true), Some(false)] {
        let (mut app, settings) = close_settings();
        let message = attached.map_or(Message::WindowOpened(settings), |attached| {
            Message::AccessibilityAttached(settings, attached)
        });
        let actual = effects(app.update(message));
        assert_eq!(
            actual,
            vec![Effect::Run(app.main_window), Effect::Close(settings)]
        );
        assert!(
            app.notice.is_none(),
            "retired attachment cannot post an error"
        );
        main_context_is_intact(&app);
    }
}

#[test]
#[cfg(target_os = "macos")]
fn retired_settings_close_cannot_flush_or_quit_the_daemon() {
    let (mut app, settings) = close_settings();
    assert!(effects(app.update(Message::CloseRequested(settings))).is_empty());
    main_context_is_intact(&app);

    drop(app.show_close_error(CloseTarget::Quit, "temporary failure".into()));
    assert!(effects(app.update(Message::CloseRequested(settings))).is_empty());
    assert!(
        app.close_error.is_some(),
        "stale close cannot dismiss a live alert"
    );
}

#[test]
#[cfg(target_os = "macos")]
fn retired_adapter_cleanup_is_not_lost_while_a_close_flush_is_pending() {
    let (mut app, retired) = close_settings();
    drop(app.update(Message::Quit));
    assert!(matches!(app.closing, Some(CloseTarget::Quit)));
    assert_eq!(
        effects(app.update(Message::AccessibilityAttached(retired, true))),
        vec![Effect::Run(app.main_window), Effect::Close(retired)],
    );
    assert!(matches!(app.closing, Some(CloseTarget::Quit)));
    assert_eq!(app.key_window, Some(app.main_window));
    assert!(app.menu_editor);
    // A legitimate, still-live completion cannot show a window while quitting.
    assert!(effects(app.update(Message::AccessibilityAttached(app.main_window, true))).is_empty());
    drop(app.update(Message::PrefsFlushed(Err("temporary failure".into()))));
    assert!(app.closing.is_none());
    assert!(app.close_error.is_some());
    assert_eq!(app.address, "unfinished camera draft");
}

#[test]
#[cfg(target_os = "macos")]
fn failed_quit_presents_an_initially_hidden_settings_alert_before_focusing_it() {
    for defer_attachment in [false, true] {
        let mut app = app();
        let mut opening = iced_runtime::task::into_stream(app.update(Message::Settings(true)))
            .expect("Settings opens its own native window");
        let Some(RuntimeAction::Window(WindowAction::Open(id, settings, reply))) =
            iced::futures::executor::block_on(opening.next())
        else {
            panic!("Settings must create a window");
        };
        assert!(!settings.visible);
        reply.send(id).unwrap();
        let Some(RuntimeAction::Output(opened @ Message::WindowOpened(_))) =
            iced::futures::executor::block_on(opening.next())
        else {
            panic!("window creation must deliver its normal completion");
        };
        assert_eq!(app.settings_window, Some(id));
        let completion = if defer_attachment {
            assert_eq!(
                effects(app.update(opened)),
                vec![Effect::Run(id), Effect::Scale(id)]
            );
            Message::AccessibilityAttached(id, true)
        } else {
            opened
        };
        drop(app.update(Message::Quit));
        assert!(effects(app.update(completion)).is_empty());
        let task = app.update(Message::PrefsFlushed(Err("temporary failure".into())));
        let attached = recovery_result(task, || {});
        let ready = mode_result(app.update(attached), window::Mode::Hidden);
        let failed = effects(app.update(ready));
        let show = failed.iter().position(|effect| *effect == Effect::Show(id));
        let focus = failed
            .iter()
            .position(|effect| *effect == Effect::Focus(id));
        assert!(
            show.is_some(),
            "failed close leaves its alert hidden: {failed:?}"
        );
        assert!(
            focus.is_some_and(|focus| show.unwrap() < focus),
            "{failed:?}"
        );
        assert!(
            failed.contains(&Effect::Run(id)),
            "the hidden window must be fitted before showing"
        );
        assert!(!failed.contains(&Effect::Exit));
        assert!(app.closing.is_none());
        assert_eq!(app.close_error.as_ref().map(|error| error.window), Some(id));
        assert_eq!(app.address, "unfinished camera draft");
    }
}

#[test]
fn failed_close_preserves_the_mode_of_a_displayed_window() {
    for mode in [window::Mode::Windowed, window::Mode::Fullscreen] {
        let mut app = app();
        app.fullscreen = mode == window::Mode::Fullscreen;
        drop(app.update(Message::Quit));
        drop(app.update(Message::PrefsFlushed(Err("temporary failure".into()))));
        let main = app.main_window;
        let ready = mode_result(
            app.update(Message::AccessibilityRecoveryAttached(main)),
            mode,
        );
        let failed = effects(app.update(ready));
        assert!(failed.contains(&Effect::Focus(app.main_window)));
        assert!(
            !failed.contains(&Effect::Show(app.main_window)),
            "{failed:?}"
        );
        assert!(!failed.contains(&Effect::Exit));
        assert_eq!(app.fullscreen, mode == window::Mode::Fullscreen);
        assert!(app.close_error.is_some());
        assert!(app.closing.is_none());
        assert_eq!(app.address, "unfinished camera draft");
    }
}

#[test]
#[cfg(target_os = "macos")]
fn recovery_attach_completion_cleans_settings_retired_after_cancel() {
    for closing in [false, true] {
        let mut app = app();
        let settings = window::Id::unique();
        app.settings_window = Some(settings);
        app.settings_open = true;
        drop(app.update(Message::WindowFocused(settings)));
        let task = app.show_close_error(CloseTarget::Quit, "temporary failure".into());
        let completion = recovery_result(task, || {
            // Hold the real Run callback until the dialog is canceled and
            // Settings has been retired/detached in the model.
            drop(app.update(Message::KeepOpen));
            drop(app.update(Message::Settings(false)));
            let main = app.main_window;
            drop(app.update(Message::WindowFocused(main)));
            if closing {
                drop(app.update(Message::Quit));
            }
        });
        assert!(matches!(completion, Message::AccessibilityRecoveryAttached(id) if id == settings));
        assert_eq!(
            effects(app.update(completion)),
            vec![Effect::Run(app.main_window), Effect::Close(settings)]
        );
        assert_eq!(app.focused_window, app.main_window);
        assert_eq!(app.key_window, Some(app.main_window));
        assert!(app.menu_editor);
        assert_eq!(app.closing.is_some(), closing);
        assert_eq!(app.address, "unfinished camera draft");
    }
}

#[test]
#[cfg(target_os = "macos")]
fn delayed_recovery_mode_cannot_revive_a_canceled_or_replaced_settings_alert() {
    for (close, reopen) in [(false, false), (true, false), (true, true)] {
        let mut app = app();
        let settings = window::Id::unique();
        app.settings_window = Some(settings);
        app.settings_open = true;
        drop(app.update(Message::WindowFocused(settings)));
        let task = app.show_close_error(CloseTarget::Quit, "temporary failure".into());
        let attached = recovery_result(task, || {});
        let mode = mode_result(app.update(attached), window::Mode::Hidden);
        drop(app.update(Message::KeepOpen));
        if close {
            drop(app.update(Message::Settings(false)));
        }
        if reopen {
            drop(app.update(Message::Settings(true)));
            assert_ne!(app.settings_window, Some(settings));
            drop(app.show_close_error(CloseTarget::Quit, "another failure".into()));
        }
        let focused = app.focused_window;
        let key = app.key_window;
        let alert = app.close_error.as_ref().map(|error| error.window);
        assert!(
            effects(app.update(mode)).is_empty(),
            "late mode cannot show/focus or select Retry"
        );
        assert_eq!(app.focused_window, focused);
        assert_eq!(app.key_window, key);
        assert_eq!(app.close_error.as_ref().map(|error| error.window), alert);
        assert!(app.closing.is_none());
        assert_eq!(app.address, "unfinished camera draft");
    }
}

#[test]
fn unknown_window_geometry_and_context_events_do_not_mutate_live_windows() {
    let mut app = app();
    let retired = window::Id::unique();
    app.key_window = Some(app.main_window);
    drop(app.update(Message::DesktopMenu(menus::DesktopInput::Focus)));
    assert!(app.desktop_menu.active());
    for message in [
        Message::WindowScale(retired, 3.0),
        Message::WindowUnfocused(retired),
        Message::AccessibilityChanged(retired),
        Message::InWindow(retired, Box::new(Message::Displaced)),
        Message::InWindow(retired, Box::new(Message::Resized(Size::new(50.0, 30.0)))),
        Message::InWindow(retired, Box::new(Message::EditorFocused(false))),
        Message::InWindow(retired, Box::new(Message::PopupOpen(true))),
        Message::InWindow(retired, Box::new(Message::Address("stale edit".into()))),
    ] {
        assert!(effects(app.update(message)).is_empty());
    }
    assert!(
        app.desktop_menu.active(),
        "retired blur cannot dismiss a live menu"
    );
    assert!(!app.window_scales.contains_key(&retired));
    assert!(!app.window_editor_focus.contains_key(&retired));
    assert!(!app.popup_open);
    assert_eq!((app.width, app.height), (1280.0, 840.0));
    main_context_is_intact(&app);
}
