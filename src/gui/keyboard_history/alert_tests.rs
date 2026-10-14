//! The failed-close alert sits outside the normal Settings/document guards.
//! Exercise that actual wrapper boundary, rather than a guard in isolation.
use super::*;
use crate::{gui::Workbench, session::SessionHandle};
use iced::advanced::renderer::Headless;

fn renderer() -> iced::Renderer {
    iced::futures::executor::block_on(iced::Renderer::new(
        iced::Font::default(),
        iced::Pixels(16.0),
        Some("tiny-skia"),
    ))
    .expect("the compiled software renderer is available")
}

fn layout(
    view: &mut Element<'_, Message>,
    tree: &mut Tree,
    renderer: &iced::Renderer,
) -> layout::Node {
    layout_sized(view, tree, renderer, Size::new(520.0, 500.0))
}

fn layout_sized(
    view: &mut Element<'_, Message>,
    tree: &mut Tree,
    renderer: &iced::Renderer,
    size: Size,
) -> layout::Node {
    tree.diff(view.as_widget());
    view.as_widget_mut()
        .layout(tree, renderer, &layout::Limits::new(Size::ZERO, size))
}

fn inner(message: &Message) -> &Message {
    match message {
        Message::InWindow(_, inner) => inner,
        message => message,
    }
}

fn apply(
    view: &mut Element<'_, Message>,
    tree: &mut Tree,
    node: &layout::Node,
    renderer: &iced::Renderer,
    action: impl Operation + 'static,
) {
    let mut action: Box<dyn Operation> = Box::new(action);
    loop {
        view.as_widget_mut()
            .operate(tree, Layout::new(node), renderer, action.as_mut());
        match action.finish() {
            operation::Outcome::Chain(next) => action = next,
            _ => break,
        }
    }
}

fn focused(
    view: &mut Element<'_, Message>,
    tree: &mut Tree,
    node: &layout::Node,
    renderer: &iced::Renderer,
) -> Option<Id> {
    let mut find = operation::focusable::find_focused();
    view.as_widget_mut().operate(
        tree,
        Layout::new(node),
        renderer,
        &mut operation::black_box(&mut find),
    );
    match find.finish() {
        operation::Outcome::Some(id) => Some(id),
        _ => None,
    }
}

#[derive(Default)]
struct Controls {
    settings: Option<Id>,
    tab_stops: Vec<Id>,
}
impl Operation for Controls {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        operate(self);
    }
    fn custom(&mut self, _: Option<&Id>, _: Rectangle, state: &mut dyn std::any::Any) {
        if let Some(control) = state.downcast_ref::<crate::gui::accessibility::Control<Message>>()
            && control.node.label() == Some("Include simulated cameras")
        {
            self.settings = Some(control.focus_id.clone());
        }
    }
    fn focusable(&mut self, id: Option<&Id>, _: Rectangle, _: &mut dyn operation::Focusable) {
        if let Some(id) = id {
            self.tab_stops.push(id.clone());
        }
    }
}

#[test]
fn settings_close_alert_routes_escape_once_and_restores_the_obscured_control() {
    let renderer = renderer();
    for alert_button in ["close-retry", "close-keep-open"] {
        let mut app = Workbench::new(SessionHandle::new(), "alert-test".into(), false, None);
        let id = window::Id::unique();
        app.settings_window = Some(id);
        app.settings_open = true;
        app.focused_window = id;
        app.address = "unfinished camera address".into();
        let mut tree = Tree::empty();
        let original_focus = {
            let mut view = app.window_view(id);
            let node = layout(&mut view, &mut tree, &renderer);
            let mut controls = Controls::default();
            view.as_widget_mut()
                .operate(&mut tree, Layout::new(&node), &renderer, &mut controls);
            let focus = controls.settings.unwrap();
            apply(
                &mut view,
                &mut tree,
                &node,
                &renderer,
                operation::focusable::focus::<()>(focus.clone()),
            );
            assert_eq!(
                focused(&mut view, &mut tree, &node, &renderer),
                Some(focus.clone())
            );
            focus
        };
        let _ = app.show_close_error(crate::gui::CloseTarget::Quit, "disk full".into());
        let mut messages = Vec::new();
        {
            let mut view = app.window_view(id);
            let node = layout(&mut view, &mut tree, &renderer);
            let mut controls = Controls::default();
            view.as_widget_mut()
                .operate(&mut tree, Layout::new(&node), &renderer, &mut controls);
            assert!(
                controls.settings.is_none(),
                "the obscured Settings control is excluded"
            );
            assert_eq!(
                controls.tab_stops,
                if cfg!(target_os = "windows") {
                    [Id::new("close-retry"), Id::new("close-keep-open")]
                } else {
                    [Id::new("close-keep-open"), Id::new("close-retry")]
                },
                "only alert actions participate in traversal"
            );
            apply(
                &mut view,
                &mut tree,
                &node,
                &renderer,
                operation::focusable::focus::<()>(Id::new(alert_button)),
            );
            assert_eq!(
                focused(&mut view, &mut tree, &node, &renderer),
                Some(Id::new(alert_button))
            );
            let mut expected = alert_button;
            for back in [false, false, true, true] {
                // Mirror FocusNext's scoped step and end-of-scope wrap.
                for _ in 0..2 {
                    let step: Box<dyn Operation> = if back {
                        Box::new(operation::focusable::focus_previous::<()>())
                    } else {
                        Box::new(operation::focusable::focus_next::<()>())
                    };
                    apply(
                        &mut view,
                        &mut tree,
                        &node,
                        &renderer,
                        operation::scope(Id::new("close-error"), step),
                    );
                    if focused(&mut view, &mut tree, &node, &renderer).is_some() {
                        break;
                    }
                }
                expected = if expected == "close-retry" {
                    "close-keep-open"
                } else {
                    "close-retry"
                };
                assert_eq!(
                    focused(&mut view, &mut tree, &node, &renderer),
                    Some(Id::new(expected)),
                    "Tab and Shift+Tab cycle only within the alert"
                );
            }
            view.as_widget_mut().update(
                &mut tree,
                &tests::named_press(keyboard::key::Named::Escape, keyboard::Modifiers::empty()),
                Layout::new(&node),
                mouse::Cursor::Unavailable,
                &renderer,
                &mut iced::advanced::clipboard::Null,
                &mut Shell::new(&mut messages),
                &Rectangle::with_size(node.size()),
            );
        }
        assert_eq!(messages.len(), 1, "no obscured guard duplicates Escape");
        assert!(
            matches!(
                &messages[0],
                Message::InWindow(window, inner) if *window == id && matches!(inner.as_ref(),
                    Message::Key {
                        chord: keys::Chord {
                            key: keys::Key::Escape,
                            mods: keys::Mods::NONE,
                        },
                        captured: true,
                    }
                )
            ),
            "the focused alert button captured Escape: {:?}",
            messages
        );
        for message in messages {
            let _ = app.update(message);
        }
        assert!(app.close_error.is_none());
        assert!(app.closing.is_none() && app.settings_open);
        assert_eq!(app.settings_window, Some(id));
        assert_eq!(app.address, "unfinished camera address");
        let mut view = app.window_view(id);
        let node = layout(&mut view, &mut tree, &renderer);
        assert_eq!(
            focused(&mut view, &mut tree, &node, &renderer),
            Some(original_focus),
            "Escape returns to the control that had focus before the failure"
        );
    }
}

#[test]
fn close_alert_redraw_retains_the_obscured_editors_menu_availability() {
    let renderer = renderer();
    let mut app = Workbench::new(SessionHandle::new(), "alert-test".into(), false, None);
    let id = app.main_window;
    app.focused_window = id;
    app.address = "unfinished camera address".into();
    let original_focus = Id::new("connect-address");
    let mut tree = Tree::empty();
    {
        let mut view = app.window_view(id);
        let node = layout(&mut view, &mut tree, &renderer);
        apply(
            &mut view,
            &mut tree,
            &node,
            &renderer,
            operation::focusable::focus::<()>(original_focus.clone()),
        );
        assert_eq!(
            focused(&mut view, &mut tree, &node, &renderer),
            Some(original_focus.clone())
        );
    }
    app.menu_editor = true;
    let _ = app.show_close_error(crate::gui::CloseTarget::Quit, "disk full".into());
    let mut redraw = Vec::new();
    {
        let mut view = app.window_view(id);
        let node = layout(&mut view, &mut tree, &renderer);
        apply(
            &mut view,
            &mut tree,
            &node,
            &renderer,
            operation::focusable::focus::<()>(Id::new("close-retry")),
        );
        view.as_widget_mut().update(
            &mut tree,
            &Event::Window(window::Event::RedrawRequested(std::time::Instant::now())),
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &renderer,
            &mut iced::advanced::clipboard::Null,
            &mut Shell::new(&mut redraw),
            &Rectangle::with_size(node.size()),
        );
    }
    assert!(
        !redraw
            .iter()
            .any(|message| matches!(inner(message), Message::EditorFocused(false))),
        "the alert must not overwrite editor metadata retained by the document"
    );
    for message in redraw {
        let _ = app.update(message);
    }
    assert!(app.menu_editor);
    let _ = app.update(Message::KeepOpen);
    assert!(
        app.menu_editor,
        "Edit commands recover without a new editor-focus event"
    );
    let mut view = app.window_view(id);
    let node = layout(&mut view, &mut tree, &renderer);
    assert_eq!(
        focused(&mut view, &mut tree, &node, &renderer),
        Some(original_focus)
    );
    assert_eq!(app.address, "unfinished camera address");
}

#[test]
fn command_period_uses_the_subscription_cancel_path_only_on_macos() {
    let renderer = renderer();
    let mut app = Workbench::new(SessionHandle::new(), "alert-test".into(), false, None);
    let id = window::Id::unique();
    app.settings_window = Some(id);
    app.settings_open = true;
    app.focused_window = id;
    app.address = "unfinished camera address".into();
    let _ = app.show_close_error(crate::gui::CloseTarget::Quit, "disk full".into());
    let mac = Os::CURRENT == Os::Mac;
    let key = keyboard::Key::Character(".".into());
    let physical_key = keyboard::key::Physical::Code(keyboard::key::Code::Period);
    let modifiers = if mac {
        keyboard::Modifiers::LOGO
    } else {
        keyboard::Modifiers::CTRL
    };
    let event = Event::Keyboard(keyboard::Event::KeyPressed {
        key: key.clone(),
        modified_key: key.clone(),
        physical_key,
        location: keyboard::Location::Standard,
        modifiers,
        text: Some(".".into()),
        repeat: false,
    });
    let mut messages = Vec::new();
    let captured = {
        let mut view = app.window_view(id);
        let mut tree = Tree::empty();
        let node = layout(&mut view, &mut tree, &renderer);
        apply(
            &mut view,
            &mut tree,
            &node,
            &renderer,
            operation::focusable::focus::<()>(Id::new("close-keep-open")),
        );
        let mut shell = Shell::new(&mut messages);
        view.as_widget_mut().update(
            &mut tree,
            &event,
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &renderer,
            &mut iced::advanced::clipboard::Null,
            &mut shell,
            &Rectangle::with_size(node.size()),
        );
        shell.is_event_captured()
    };
    assert_eq!(captured, mac);
    assert!(
        messages.is_empty(),
        "the keyboard subscription owns modified shortcuts"
    );
    // This is the same window-tagged chord the event subscription constructs
    // for a modified key; unlike bare Escape it does not discard this event.
    let chord = keys::chord(&key, physical_key, modifiers, Os::CURRENT).unwrap();
    let _ = app.update(Message::InWindow(
        id,
        Box::new(Message::Key { chord, captured }),
    ));
    assert_eq!(app.close_error.is_none(), mac);
    assert!(app.closing.is_none() && app.settings_open);
    assert_eq!(app.settings_window, Some(id));
    assert_eq!(app.address, "unfinished camera address");
}

#[test]
fn actual_close_alert_return_uses_default_retry_and_space_uses_focused_cancel() {
    let renderer = renderer();
    for focus in ["close-keep-open", "close-retry", "none"] {
        let mut app = Workbench::new(SessionHandle::new(), "alert-test".into(), false, None);
        let id = window::Id::unique();
        app.settings_window = Some(id);
        app.settings_open = true;
        app.focused_window = id;
        let _ = app.show_close_error(crate::gui::CloseTarget::Quit, "disk full".into());
        let mut view = app.window_view(id);
        let mut tree = Tree::empty();
        let node = layout(&mut view, &mut tree, &renderer);
        if focus != "none" {
            apply(
                &mut view,
                &mut tree,
                &node,
                &renderer,
                operation::focusable::focus::<()>(Id::new(focus)),
            );
        }
        let mut messages = Vec::new();
        view.as_widget_mut().update(
            &mut tree,
            &tests::named_press(keyboard::key::Named::Enter, keyboard::Modifiers::empty()),
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &renderer,
            &mut iced::advanced::clipboard::Null,
            &mut Shell::new(&mut messages),
            &Rectangle::with_size(node.size()),
        );
        assert_eq!(messages.len(), 1);
        let expected_cancel = Os::CURRENT != Os::Mac && focus == "close-keep-open";
        assert!(matches!(inner(&messages[0]), Message::KeepOpen) == expected_cancel);
        assert!(matches!(inner(&messages[0]), Message::RetryClose) != expected_cancel);
        apply(
            &mut view,
            &mut tree,
            &node,
            &renderer,
            operation::focusable::focus::<()>(Id::new("close-keep-open")),
        );
        messages.clear();
        view.as_widget_mut().update(
            &mut tree,
            &tests::named_press(keyboard::key::Named::Space, keyboard::Modifiers::empty()),
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &renderer,
            &mut iced::advanced::clipboard::Null,
            &mut Shell::new(&mut messages),
            &Rectangle::with_size(node.size()),
        );
        assert!(messages.is_empty());
        let released = Event::Keyboard(keyboard::Event::KeyReleased {
            key: keyboard::Key::Named(keyboard::key::Named::Space),
            modified_key: keyboard::Key::Named(keyboard::key::Named::Space),
            physical_key: keyboard::key::Physical::Unidentified(
                keyboard::key::NativeCode::Unidentified,
            ),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
        });
        view.as_widget_mut().update(
            &mut tree,
            &released,
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &renderer,
            &mut iced::advanced::clipboard::Null,
            &mut Shell::new(&mut messages),
            &Rectangle::with_size(node.size()),
        );
        assert!(
            matches!(messages.as_slice(), [message] if matches!(inner(message), Message::KeepOpen))
        );
    }
}

#[test]
fn long_close_error_keeps_native_dialog_actions_inside_small_scaled_viewports() {
    use crate::gui::accessibility::{Role, snapshot_widget};
    use crate::gui::prefs::UiScale;

    let renderer = renderer();
    for scale in UiScale::ALL {
        for physical in [
            Size::new(520.0, 500.0),
            Size::new(400.0, 300.0),
            Size::new(250.0, 300.0),
        ] {
            let factor = scale.factor();
            let viewport = Size::new(physical.width / factor, physical.height / factor);
            let mut app = Workbench::new(SessionHandle::new(), "alert-test".into(), false, None);
            let id = window::Id::unique();
            app.settings_window = Some(id);
            app.settings_open = true;
            app.focused_window = id;
            app.ui_scale = scale;
            let details =
                "Unable to create the settings file because the filesystem is full. ".repeat(100);
            let _ = app.show_close_error(crate::gui::CloseTarget::Quit, details);
            let mut view = app.window_view(id);
            let mut tree = Tree::empty();
            let node = layout_sized(&mut view, &mut tree, &renderer, viewport);
            let (snapshot, _, _) = snapshot_widget(
                view.as_widget_mut(),
                &mut tree,
                Layout::new(&node),
                &renderer,
            );
            let (dialog_id, dialog) = snapshot
                .nodes
                .iter()
                .find(|(_, node)| node.role() == Role::AlertDialog)
                .expect("the failure is a native alert dialog");
            assert!(dialog.is_modal());
            assert_eq!(dialog.label(), Some("Could Not Save Settings"));
            assert!(snapshot.nodes.iter().any(|(_, node)| {
                node.role() == Role::Heading && node.label() == Some("Could Not Save Settings")
            }));
            let buttons: Vec<_> = snapshot
                .nodes
                .iter()
                .filter(|(_, node)| node.role() == Role::Button)
                .collect();
            assert_eq!(buttons.len(), 2, "only the two alert actions are exposed");
            let cancel = buttons
                .iter()
                .find(|(_, button)| button.label() == Some("Cancel"))
                .expect("the cancellation action uses its standard native label")
                .1
                .bounds()
                .unwrap();
            let retry = buttons
                .iter()
                .find(|(_, button)| button.label() == Some("Retry"))
                .expect("the default action is named Retry")
                .1
                .bounds()
                .unwrap();
            if (retry.y0 - cancel.y0).abs() < 1.0 {
                if cfg!(target_os = "windows") {
                    assert!(
                        retry.x1 < cancel.x0,
                        "Windows places the safe cancellation last"
                    );
                } else {
                    assert!(
                        retry.x0 > cancel.x1,
                        "macOS and GNOME place cancellation first"
                    );
                }
            } else {
                assert!(
                    retry.y1 < cancel.y0,
                    "the default action is first in a narrow stack"
                );
            }
            for (button_id, button) in buttons {
                assert!(
                    dialog.children().contains(button_id),
                    "each action belongs to the dialog"
                );
                assert_eq!(
                    snapshot
                        .nodes
                        .iter()
                        .filter(|(_, parent)| parent.children().contains(button_id))
                        .count(),
                    1,
                    "native actions have a single semantic parent"
                );
                let bounds = button.bounds().unwrap();
                assert!(
                    bounds.x0 >= 0.0
                        && bounds.y0 >= 0.0
                        && bounds.x1 <= f64::from(viewport.width) + 0.5
                        && bounds.y1 <= f64::from(viewport.height) + 0.5,
                    "{} stays inside {physical:?} at {scale:?}: {bounds:?}",
                    button.label().unwrap(),
                );
                assert!(button.supports_action(accesskit::Action::Click));
            }
            assert_eq!(
                snapshot
                    .nodes
                    .iter()
                    .filter(|(_, node)| node.children().contains(dialog_id))
                    .count(),
                1
            );
        }
    }
}
