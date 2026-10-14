//! Newly added reports share the real sheet/window boundary with existing
//! editors. Cancellation must close only the report and recover that editor.
use super::window_focus_tests::{editor, event};
use super::*;
use crate::{
    gui::{UiScale, Workbench, accessibility},
    session::SessionHandle,
};

fn layout<'a>(
    app: &'a Workbench,
    tree: &mut Tree,
    renderer: &iced::Renderer,
) -> (Element<'a, Message>, layout::Node) {
    let mut view = app.window_view(app.main_window);
    tree.diff(view.as_widget());
    let node = view.as_widget_mut().layout(
        tree,
        renderer,
        &layout::Limits::new(Size::ZERO, Size::new(app.width, app.height)),
    );
    (view, node)
}

#[test]
fn analytics_and_network_cancel_from_actual_compact_window_views_preserves_draft_focus() {
    let renderer = crate::gui::destinations::layout_tests::renderer();
    for scale in [UiScale::Standard, UiScale::Larger] {
        for analytics in [true, false] {
            for cancel in ["return", "escape", "close-window", "window-close"] {
                let mut app = Workbench::new(
                    SessionHandle::new(),
                    "report-keyboard-test".into(),
                    false,
                    None,
                );
                app.width = 900.0 / scale.factor();
                app.height = 620.0 / scale.factor();
                app.ui_scale = scale;
                app.address = "unfinished camera address".into();
                let main = app.main_window;
                let mut tree = Tree::empty();
                let original = {
                    let (mut view, node) = layout(&app, &mut tree, &renderer);
                    view.as_widget_mut().operate(
                        &mut tree,
                        Layout::new(&node),
                        &renderer,
                        &mut operation::focusable::focus::<()>(Id::new("connect-address")),
                    );
                    view.as_widget_mut().operate(
                        &mut tree,
                        Layout::new(&node),
                        &renderer,
                        &mut operation::text_input::select_all::<()>(Id::new("connect-address")),
                    );
                    editor(&mut view, &mut tree, &node, &renderer)
                };
                assert_eq!(original.0, original.2);
                app.analytics.open = analytics;
                app.network_open = !analytics;
                app.sync_animations();
                let messages = {
                    let (mut view, node) = layout(&app, &mut tree, &renderer);
                    view.as_widget_mut().operate(
                        &mut tree,
                        Layout::new(&node),
                        &renderer,
                        &mut operation::focusable::focus::<()>(Id::new("sheet-close")),
                    );
                    let (snapshot, _, _) = accessibility::snapshot_widget(
                        view.as_widget_mut(),
                        &mut tree,
                        Layout::new(&node),
                        &renderer,
                    );
                    let close = snapshot
                        .nodes
                        .iter()
                        .find(|(_, node)| {
                            node.label()
                                == Some(if analytics {
                                    "Close Session analytics"
                                } else {
                                    "Close Camera network"
                                })
                        })
                        .unwrap();
                    assert_eq!(snapshot.focus, close.0);
                    let bounds = close.1.bounds().unwrap();
                    assert!(
                        bounds.x0 >= 0.0
                            && bounds.y0 >= 0.0
                            && bounds.x1 <= app.width as f64
                            && bounds.y1 <= app.height as f64,
                        "compact header action is visible"
                    );
                    assert!(
                        !snapshot
                            .nodes
                            .iter()
                            .any(|(_, node)| node.label() == Some("Camera address")),
                        "the obscured editor is excluded from native traversal"
                    );
                    match cancel {
                        "return" => event(
                            &mut view,
                            &mut tree,
                            &node,
                            &renderer,
                            tests::named_press(
                                keyboard::key::Named::Enter,
                                keyboard::Modifiers::empty(),
                            ),
                        ),
                        "escape" => event(
                            &mut view,
                            &mut tree,
                            &node,
                            &renderer,
                            tests::named_press(
                                keyboard::key::Named::Escape,
                                keyboard::Modifiers::empty(),
                            ),
                        ),
                        "window-close" => {
                            assert!(
                                event(
                                    &mut view,
                                    &mut tree,
                                    &node,
                                    &renderer,
                                    Event::Window(window::Event::CloseRequested)
                                )
                                .is_empty()
                            );
                            vec![Message::CloseRequested(main)]
                        }
                        "close-window" => {
                            let key = keyboard::Key::Character("w".into());
                            let modifiers = if Os::CURRENT == Os::Mac {
                                keyboard::Modifiers::LOGO
                            } else {
                                keyboard::Modifiers::CTRL
                            };
                            let physical_key =
                                keyboard::key::Physical::Code(keyboard::key::Code::KeyW);
                            let mut messages = Vec::new();
                            let mut shell = Shell::new(&mut messages);
                            view.as_widget_mut().update(
                                &mut tree,
                                &Event::Keyboard(keyboard::Event::KeyPressed {
                                    key: key.clone(),
                                    modified_key: key.clone(),
                                    physical_key,
                                    location: keyboard::Location::Standard,
                                    modifiers,
                                    text: Some("w".into()),
                                    repeat: false,
                                }),
                                Layout::new(&node),
                                mouse::Cursor::Unavailable,
                                &renderer,
                                &mut iced::advanced::clipboard::Null,
                                &mut shell,
                                &Rectangle::with_size(node.size()),
                            );
                            assert!(shell.is_event_captured());
                            assert!(messages.is_empty());
                            // The asynchronous window-tagged subscription owns
                            // modified shortcuts, as it does native Close.
                            vec![Message::InWindow(
                                main,
                                Box::new(Message::Key {
                                    chord: keys::chord(&key, physical_key, modifiers, Os::CURRENT)
                                        .unwrap(),
                                    captured: true,
                                }),
                            )]
                        }
                        _ => unreachable!(),
                    }
                };
                assert_eq!(messages.len(), 1, "{cancel} dispatches exactly once");
                for message in messages {
                    let _ = app.update(message);
                }
                assert!(!app.analytics.open && !app.network_open);
                assert!(
                    app.closing.is_none(),
                    "canceling the report keeps the workbench open"
                );
                assert_eq!(app.address, "unfinished camera address");
                let (mut view, node) = layout(&app, &mut tree, &renderer);
                let restored = editor(&mut view, &mut tree, &node, &renderer);
                assert_eq!(restored.0, original.0);
                assert_eq!(restored.2, original.0);
                assert_eq!(restored.1.value(), original.1.value());
                assert_eq!(restored.1.text_selection(), original.1.text_selection());
            }
        }
    }
}
