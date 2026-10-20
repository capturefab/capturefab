//! The compact Activity pane's pinned Copy action is its keyboard entry point.
//! Page and arrow keys must reveal the real bottom-anchored history from there.
use super::*;
use iced::advanced::{
    Layout, Shell, layout,
    widget::{Id, Operation, Tree, operation},
};
use iced::widget::column;
use iced::{Rectangle, Vector};

#[derive(Default)]
struct History {
    viewport: Rectangle,
    content: Rectangle,
    offset: Vector,
    copy: Option<Id>,
}
impl Operation for History {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        operate(self);
    }
    fn scrollable(
        &mut self,
        _: Option<&Id>,
        bounds: Rectangle,
        content: Rectangle,
        translation: Vector,
        _: &mut dyn operation::Scrollable,
    ) {
        self.viewport = bounds;
        self.content = content;
        self.offset = translation;
    }
    fn custom(&mut self, _: Option<&Id>, _: Rectangle, state: &mut dyn std::any::Any) {
        if let Some(control) = state.downcast_ref::<accessibility::Control<Message>>()
            && control.node.label() == Some("Copy activity log")
        {
            self.copy = Some(control.focus_id.clone());
        }
    }
}
fn history(
    view: &mut Element<'_, Message>,
    tree: &mut Tree,
    node: &layout::Node,
    renderer: &iced::Renderer,
) -> History {
    let mut history = History::default();
    view.as_widget_mut()
        .operate(tree, Layout::new(node), renderer, &mut history);
    history
}
fn event(
    view: &mut Element<'_, Message>,
    tree: &mut Tree,
    node: &layout::Node,
    renderer: &iced::Renderer,
    event: iced::Event,
) -> (bool, Vec<Message>) {
    let mut messages = Vec::new();
    let mut shell = Shell::new(&mut messages);
    view.as_widget_mut().update(
        tree,
        &event,
        Layout::new(node),
        iced::mouse::Cursor::Unavailable,
        renderer,
        &mut iced::advanced::clipboard::Null,
        &mut shell,
        &Rectangle::with_size(node.size()),
    );
    (shell.is_event_captured(), messages)
}
fn key(named: keyboard::key::Named, modifiers: keyboard::Modifiers) -> iced::Event {
    iced::Event::Keyboard(keyboard::Event::KeyPressed {
        key: keyboard::Key::Named(named),
        modified_key: keyboard::Key::Named(named),
        physical_key: keyboard::key::Physical::Unidentified(
            keyboard::key::NativeCode::Unidentified,
        ),
        location: keyboard::Location::Standard,
        modifiers,
        text: None,
        repeat: false,
    })
}
fn app() -> Workbench {
    let mut app = Workbench::new(
        crate::session::SessionHandle::new(),
        "activity-keyboard-test".into(),
        true,
        None,
    );
    app.snapshot.activity = (0..24)
        .map(|id| {
            serde_json::from_value(serde_json::json!({
                "id": id, "origin": "client", "actor": {
                    "id": format!("agent-{id}"), "name": format!("Agent {id}"), "kind": "agent"
                }, "operation": "capture", "camera": "sim:0", "feature": null,
                "label": format!("Capture frame {id}"), "state": "succeeded", "submitted_at_ms": 1,
                "started_at_ms": 1, "finished_at_ms": 2, "detail": format!("Saved capture {id} to /tmp/activity/capture-{id}.png"),
                "file_count": 1, "files": [], "job": null
            }))
            .unwrap()
        })
        .collect();
    app
}

#[test]
fn focused_activity_action_scrolls_history_in_visual_order_and_keeps_actions_pinned() {
    use keyboard::key::Named;
    let renderer = crate::gui::destinations::layout_tests::renderer();
    let app = app();
    let mut view = app.activity(Palette::of(false));
    let mut tree = Tree::new(view.as_widget());
    let node = view.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, Size::new(520.0, 171.0)),
    );
    let before = history(&mut view, &mut tree, &node, &renderer);
    assert!(before.content.height > before.viewport.height * 2.0);
    // Scrollable rounds its final translation to a logical pixel. Date
    // captions can give the content a fractional height; compare the actual
    // snapped end position rather than an unreachable fractional offset.
    let max = (before.content.height - before.viewport.height).round();
    assert!(
        (before.offset.y - max).abs() < 0.01,
        "new Activity views show the latest entries: offset {:?}, content {:?}, viewport {:?}",
        before.offset,
        before.content,
        before.viewport
    );
    view.as_widget_mut().operate(
        &mut tree,
        Layout::new(&node),
        &renderer,
        &mut operation::focusable::focus::<()>(before.copy.unwrap()),
    );
    let copy_bounds = |view: &mut Element<'_, Message>, tree: &mut Tree| {
        let (snapshot, _, _) = accessibility::snapshot_widget(
            view.as_widget_mut(),
            tree,
            Layout::new(&node),
            &renderer,
        );
        snapshot
            .nodes
            .into_iter()
            .find(|(_, control)| control.label() == Some("Copy activity log"))
            .unwrap()
            .1
            .bounds()
            .unwrap()
    };
    let pinned = copy_bounds(&mut view, &mut tree);
    assert!(
        pinned.x0 >= 0.0
            && pinned.y0 >= 0.0
            && pinned.x1 <= node.size().width as f64
            && pinned.y1 <= node.size().height as f64,
        "the compact pane keeps its Copy action fully inside its bounds"
    );
    for (named, older) in [
        (Named::PageUp, true),
        (Named::ArrowUp, true),
        (Named::Home, true),
        (Named::ArrowDown, false),
        (Named::PageDown, false),
        (Named::End, false),
    ] {
        let before = history(&mut view, &mut tree, &node, &renderer).offset.y;
        let (captured, messages) = event(
            &mut view,
            &mut tree,
            &node,
            &renderer,
            key(named, keyboard::Modifiers::empty()),
        );
        let after = history(&mut view, &mut tree, &node, &renderer).offset.y;
        assert!(
            if older {
                after < before
            } else {
                after > before
            },
            "{named:?} reaches {} Activity entries",
            if older { "older" } else { "newer" }
        );
        assert!(captured);
        assert!(
            messages.is_empty(),
            "scrolling does not activate Copy or an app action"
        );
        assert_eq!(copy_bounds(&mut view, &mut tree), pinned);
    }
    assert!((history(&mut view, &mut tree, &node, &renderer).offset.y - max).abs() < 0.01);
    let (captured, messages) = event(
        &mut view,
        &mut tree,
        &node,
        &renderer,
        key(Named::Enter, keyboard::Modifiers::empty()),
    );
    assert!(captured);
    assert!(matches!(messages.as_slice(), [Message::CopyLog]));
}

#[test]
fn activity_scrolling_does_not_take_keys_from_other_controls_or_inactive_content() {
    use keyboard::key::Named;
    let renderer = crate::gui::destinations::layout_tests::renderer();
    let app = app();
    let make = |blocked| {
        keyboard_history::background(
            column![
                button(text("Another pane"))
                    .id("other-action")
                    .on_press(Message::ToggleInspector),
                text_input("", "untouched draft")
                    .id("other-editor")
                    .on_input(Message::Search),
                app.activity(Palette::of(false)),
            ]
            .into(),
            blocked,
        )
    };
    let mut tree = Tree::empty();
    let layout = |view: &mut Element<'_, Message>, tree: &mut Tree| {
        tree.diff(view.as_widget());
        view.as_widget_mut().layout(
            tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(520.0, 270.0)),
        )
    };
    let mut view = make(false);
    let node = layout(&mut view, &mut tree);
    let initial = history(&mut view, &mut tree, &node, &renderer);
    let at_end = initial.offset.y;
    for id in ["other-action", "other-editor"] {
        view.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut operation::focusable::focus::<()>(Id::new(id)),
        );
        let (_, messages) = event(
            &mut view,
            &mut tree,
            &node,
            &renderer,
            key(Named::PageUp, keyboard::Modifiers::empty()),
        );
        assert!(messages.is_empty());
        assert_eq!(
            history(&mut view, &mut tree, &node, &renderer).offset.y,
            at_end
        );
    }
    let (captured, messages) = event(
        &mut view,
        &mut tree,
        &node,
        &renderer,
        key(Named::ArrowLeft, keyboard::Modifiers::empty()),
    );
    assert!(captured, "the actual editor retains its caret keys");
    assert!(messages.is_empty());
    assert_eq!(
        history(&mut view, &mut tree, &node, &renderer).offset.y,
        at_end
    );
    view.as_widget_mut().operate(
        &mut tree,
        Layout::new(&node),
        &renderer,
        &mut operation::focusable::unfocus::<()>(),
    );
    assert!(
        !event(
            &mut view,
            &mut tree,
            &node,
            &renderer,
            key(Named::PageUp, keyboard::Modifiers::empty())
        )
        .0
    );
    assert_eq!(
        history(&mut view, &mut tree, &node, &renderer).offset.y,
        at_end
    );
    view.as_widget_mut().operate(
        &mut tree,
        Layout::new(&node),
        &renderer,
        &mut operation::focusable::focus::<()>(initial.copy.unwrap()),
    );
    for modifiers in [
        keyboard::Modifiers::CTRL,
        keyboard::Modifiers::LOGO,
        keyboard::Modifiers::ALT,
        keyboard::Modifiers::SHIFT,
    ] {
        event(
            &mut view,
            &mut tree,
            &node,
            &renderer,
            key(Named::PageUp, modifiers),
        );
        assert_eq!(
            history(&mut view, &mut tree, &node, &renderer).offset.y,
            at_end
        );
    }
    event(
        &mut view,
        &mut tree,
        &node,
        &renderer,
        iced::Event::Window(window::Event::Unfocused),
    );
    assert!(
        !event(
            &mut view,
            &mut tree,
            &node,
            &renderer,
            key(Named::PageUp, keyboard::Modifiers::empty())
        )
        .0
    );
    assert_eq!(
        history(&mut view, &mut tree, &node, &renderer).offset.y,
        at_end
    );
    event(
        &mut view,
        &mut tree,
        &node,
        &renderer,
        iced::Event::Window(window::Event::Focused),
    );
    drop(view);
    {
        let mut view = make(true);
        let node = layout(&mut view, &mut tree);
        event(
            &mut view,
            &mut tree,
            &node,
            &renderer,
            key(Named::Home, keyboard::Modifiers::empty()),
        );
    }
    let mut view = make(false);
    let node = layout(&mut view, &mut tree);
    assert_eq!(
        history(&mut view, &mut tree, &node, &renderer).offset.y,
        at_end,
        "a sheet obscuring Activity blocks its keys"
    );
    assert!(
        event(
            &mut view,
            &mut tree,
            &node,
            &renderer,
            key(Named::PageUp, keyboard::Modifiers::empty())
        )
        .0
    );
    assert!(history(&mut view, &mut tree, &node, &renderer).offset.y < at_end);
}
