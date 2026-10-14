//! Native widget events can precede the asynchronous window subscription.
//! Exercise that ordering through the actual two-window views and menu model.
use super::*;
use crate::{
    gui::{Workbench, accessibility, menus},
    session::SessionHandle,
};

fn layout(
    view: &mut Element<'_, Message>,
    tree: &mut Tree,
    renderer: &iced::Renderer,
) -> layout::Node {
    tree.diff(view.as_widget());
    view.as_widget_mut().layout(
        tree,
        renderer,
        &layout::Limits::new(Size::ZERO, Size::new(900.0, 620.0)),
    )
}

pub(super) fn event(
    view: &mut Element<'_, Message>,
    tree: &mut Tree,
    node: &layout::Node,
    renderer: &iced::Renderer,
    event: Event,
) -> Vec<Message> {
    let mut messages = Vec::new();
    view.as_widget_mut().update(
        tree,
        &event,
        Layout::new(node),
        mouse::Cursor::Unavailable,
        renderer,
        &mut iced::advanced::clipboard::Null,
        &mut Shell::new(&mut messages),
        &Rectangle::with_size(node.size()),
    );
    messages
}

fn redraw(
    view: &mut Element<'_, Message>,
    tree: &mut Tree,
    node: &layout::Node,
    renderer: &iced::Renderer,
) -> Vec<Message> {
    event(
        view,
        tree,
        node,
        renderer,
        Event::Window(window::Event::RedrawRequested(std::time::Instant::now())),
    )
}

pub(super) fn editor(
    view: &mut Element<'_, Message>,
    tree: &mut Tree,
    node: &layout::Node,
    renderer: &iced::Renderer,
) -> (
    accessibility::NodeId,
    accessibility::Node,
    accessibility::NodeId,
) {
    let (snapshot, _, _) =
        accessibility::snapshot_widget(view.as_widget_mut(), tree, Layout::new(node), renderer);
    let focused = snapshot.focus;
    let (id, node) = snapshot
        .nodes
        .into_iter()
        .find(|(_, node)| node.label() == Some("Camera address"))
        .expect("the actual address editor is in the main window");
    (id, node, focused)
}

fn edits_enabled(app: &Workbench, enabled: bool) {
    let edits: Vec<_> = app
        .command_menus()
        .into_iter()
        .flat_map(|menu| menu.items.into_iter().flatten())
        .filter(|item| matches!(item.command, menus::Command::Edit(_)))
        .collect();
    assert_eq!(edits.len(), 4);
    for item in edits {
        assert_eq!(item.enabled, enabled, "{} availability", item.title);
    }
}

#[test]
fn delayed_window_activation_preserves_editor_menu_context_and_selection() {
    let renderer = crate::gui::destinations::layout_tests::renderer();
    for close_settings in [false, true] {
        let mut app = Workbench::new(
            SessionHandle::new(),
            "window-focus-test".into(),
            false,
            None,
        );
        app.address = "unfinished camera draft".into();
        let main = app.main_window;
        let settings = window::Id::unique();
        let mut tree = Tree::empty();
        let original = {
            let mut view = app.window_view(main);
            let node = layout(&mut view, &mut tree, &renderer);
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
        assert_eq!(original.1.value(), Some(app.address.as_str()));
        let selection = original.1.text_selection().cloned().unwrap();
        assert_ne!(selection.anchor, selection.focus);

        app.settings_window = Some(settings);
        app.settings_open = true;
        let _ = app.update(Message::WindowFocused(settings));
        // Settings has its own real guard and controls, rather than borrowing
        // the still-focused editor's menu context from the main window.
        let settings_messages = {
            let mut view = app.window_view(settings);
            let mut settings_tree = Tree::empty();
            let node = layout(&mut view, &mut settings_tree, &renderer);
            redraw(&mut view, &mut settings_tree, &node, &renderer)
        };
        for message in settings_messages {
            let _ = app.update(message);
        }
        edits_enabled(&app, false);

        // The widget focus event and first redraw are delivered before the
        // application's asynchronous WindowFocused(main) subscription message.
        let messages = {
            let mut view = app.window_view(main);
            let node = layout(&mut view, &mut tree, &renderer);
            let messages = event(
                &mut view,
                &mut tree,
                &node,
                &renderer,
                Event::Window(window::Event::Focused),
            );
            assert!(messages.is_empty());
            redraw(&mut view, &mut tree, &node, &renderer)
        };
        assert!(messages.iter().any(|message| matches!(message,
            Message::InWindow(id, message) if *id == main && matches!(message.as_ref(), Message::EditorFocused(true))
        )));
        for message in messages {
            let _ = app.update(message);
        }
        edits_enabled(&app, false);
        if close_settings {
            let _ = app.update(Message::Settings(false));
        }
        let _ = app.update(Message::WindowFocused(main));
        edits_enabled(&app, true);
        {
            let mut view = app.window_view(main);
            let node = layout(&mut view, &mut tree, &renderer);
            let messages = redraw(&mut view, &mut tree, &node, &renderer);
            assert!(!messages.iter().any(|message| matches!(message,
                Message::InWindow(_, message) if matches!(message.as_ref(), Message::EditorFocused(_))
            )), "unchanged widget focus emits no second metadata report");
            let current = editor(&mut view, &mut tree, &node, &renderer);
            assert_eq!(current.0, original.0);
            assert_eq!(current.2, original.0);
            assert_eq!(current.1.value(), original.1.value());
            assert_eq!(current.1.text_selection(), Some(&selection));
        }
        edits_enabled(&app, true);
        assert_eq!(app.address, "unfinished camera draft");
    }
}
