//! Read-only reports still need keyboard access to content below the viewport.
use super::*;
use iced::advanced::{
    Layout, Shell, layout,
    widget::{Id, Operation, Tree, operation},
};
use iced::widget::column;
use iced::{Rectangle, Vector};

#[derive(Default)]
struct ScrollGeometry {
    viewport: Rectangle,
    content: Rectangle,
    offset: Vector,
}
impl Operation for ScrollGeometry {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        operate(self);
    }
    fn scrollable(
        &mut self,
        id: Option<&Id>,
        bounds: Rectangle,
        content: Rectangle,
        translation: Vector,
        _: &mut dyn operation::Scrollable,
    ) {
        if id == Some(&Id::new(SHEET_BODY)) {
            self.viewport = bounds;
            self.content = content;
            self.offset = translation;
        }
    }
}
fn geometry(
    view: &mut Element<'_, Message>,
    tree: &mut Tree,
    node: &layout::Node,
    renderer: &iced::Renderer,
) -> ScrollGeometry {
    let mut geometry = ScrollGeometry::default();
    view.as_widget_mut()
        .operate(tree, Layout::new(node), renderer, &mut geometry);
    geometry
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
    let captured = shell.is_event_captured();
    (captured, messages)
}
fn focus(
    view: &mut Element<'_, Message>,
    tree: &mut Tree,
    node: &layout::Node,
    renderer: &iced::Renderer,
    id: &'static str,
) {
    view.as_widget_mut().operate(
        tree,
        Layout::new(node),
        renderer,
        &mut operation::focusable::focus::<()>(Id::new(id)),
    );
}
fn focus_role(
    view: &mut Element<'_, Message>,
    tree: &mut Tree,
    node: &layout::Node,
    renderer: &iced::Renderer,
    role: accessibility::Role,
) {
    struct Find {
        role: accessibility::Role,
        id: Option<Id>,
    }
    impl Operation for Find {
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
            operate(self);
        }
        fn custom(&mut self, _: Option<&Id>, _: Rectangle, state: &mut dyn std::any::Any) {
            if let Some(control) = state.downcast_ref::<accessibility::Control<Message>>()
                && control.node.role() == self.role
                && self.id.is_none()
            {
                self.id = Some(control.focus_id.clone());
            }
        }
    }
    let mut find = Find { role, id: None };
    view.as_widget_mut()
        .operate(tree, Layout::new(node), renderer, &mut find);
    view.as_widget_mut().operate(
        tree,
        Layout::new(node),
        renderer,
        &mut operation::focusable::focus::<()>(
            find.id.expect("the actual compound control exposes focus"),
        ),
    );
}
fn app(network: bool) -> Workbench {
    let mut app = Workbench::new(
        crate::session::SessionHandle::new(),
        "keyboard-scroll-test".into(),
        true,
        None,
    );
    app.height = 400.0;
    app.help_open = !network;
    app.sheets.more = true;
    app.network_open = network;
    app.snapshot.network_scan = Some(crate::network::NetworkScan {
        interfaces: Vec::new(),
        cameras: Vec::new(),
        warnings: (0..20).map(|index| format!("Adapter {index} needs attention. Check its Ethernet cable, subnet and connection speed before retrying the scan.")).collect(),
        scanned_at_ms: 0,
    });
    app
}
#[test]
fn read_only_network_and_help_body_scroll_with_keys_while_actions_remain_pinned() {
    use keyboard::key::Named;
    let renderer = crate::gui::destinations::layout_tests::renderer();
    for network in [true, false] {
        let app = app(network);
        let mut view = app.sheets(space().into(), Palette::of(false));
        let mut tree = Tree::new(view.as_widget());
        let node = view.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(480.0, 400.0)),
        );
        focus(&mut view, &mut tree, &node, &renderer, "sheet-close");
        let before = geometry(&mut view, &mut tree, &node, &renderer);
        assert!(
            before.content.height > before.viewport.height,
            "the actual report overflows"
        );
        let (captured, _) = event(
            &mut view,
            &mut tree,
            &node,
            &renderer,
            key(Named::PageDown, keyboard::Modifiers::empty()),
        );
        let after = geometry(&mut view, &mut tree, &node, &renderer);
        assert!(
            after.offset.y > before.offset.y,
            "PageDown reaches lower content in the actual {} sheet",
            if network { "network" } else { "Help" }
        );
        assert!(captured);
        let fixed_actions = |view: &mut Element<'_, Message>, tree: &mut Tree| {
            let (snapshot, _, _) = accessibility::snapshot_widget(
                view.as_widget_mut(),
                tree,
                Layout::new(&node),
                &renderer,
            );
            snapshot
                .nodes
                .into_iter()
                .filter(|(_, control)| {
                    control.role() == accessibility::Role::Button
                        && matches!(
                            control.label(),
                            Some("Close Camera network" | "Close Quick guide" | "Scan network")
                        )
                })
                .map(|(id, control)| (id, control.bounds().unwrap()))
                .collect::<Vec<_>>()
        };
        let actions = fixed_actions(&mut view, &mut tree);
        for (named, lower) in [
            (Named::ArrowDown, true),
            (Named::End, true),
            (Named::ArrowUp, false),
            (Named::PageUp, false),
            (Named::Home, false),
        ] {
            let before = geometry(&mut view, &mut tree, &node, &renderer).offset.y;
            let (captured, _) = event(
                &mut view,
                &mut tree,
                &node,
                &renderer,
                key(named, keyboard::Modifiers::empty()),
            );
            let after = geometry(&mut view, &mut tree, &node, &renderer).offset.y;
            assert!(captured);
            assert!(
                if lower {
                    after > before
                } else {
                    after < before
                },
                "{named:?} scrolls the actual body in the expected direction"
            );
            assert_eq!(
                fixed_actions(&mut view, &mut tree),
                actions,
                "header/footer actions remain pinned"
            );
        }
        assert_eq!(
            geometry(&mut view, &mut tree, &node, &renderer).offset.y,
            0.0
        );
    }
}

#[test]
fn sheet_scroll_preserves_editor_popup_and_radio_keyboard_priority() {
    use keyboard::key::Named;
    let renderer = crate::gui::destinations::layout_tests::renderer();
    let choices = vec![
        (
            "Light",
            Some(Message::Appearance(crate::gui::Appearance::Light)),
            String::new(),
        ),
        (
            "Dark",
            Some(Message::Appearance(crate::gui::Appearance::Dark)),
            String::new(),
        ),
    ];
    let mut body = column![
        text_input("Address", "draft")
            .id("scroll-editor")
            .on_input(Message::Address),
        pick_list(
            prefs::UiScale::ALL,
            Some(prefs::UiScale::Standard),
            Message::UiScale
        )
        .id("scroll-choice"),
        segmented("Test choices", choices, 0, 0.0),
    ]
    .spacing(12);
    for _ in 0..20 {
        body = body.push(text("Additional report content"));
    }
    let mut view: Element<'_, Message> =
        sheet_frame("Priority", Message::Help(false), body, Palette::of(false))
            .size(480.0, 300.0)
            .into();
    let mut tree = Tree::new(view.as_widget());
    let node = view.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, Size::new(480.0, 300.0)),
    );
    focus(&mut view, &mut tree, &node, &renderer, "scroll-editor");
    let (captured, _) = event(
        &mut view,
        &mut tree,
        &node,
        &renderer,
        key(Named::Home, keyboard::Modifiers::empty()),
    );
    assert!(captured, "the editor owns Home");
    assert_eq!(
        geometry(&mut view, &mut tree, &node, &renderer).offset.y,
        0.0
    );
    focus(&mut view, &mut tree, &node, &renderer, "scroll-choice");
    let (captured, _) = event(
        &mut view,
        &mut tree,
        &node,
        &renderer,
        key(Named::ArrowDown, keyboard::Modifiers::empty()),
    );
    assert!(captured, "the combo box owns its arrow");
    let (captured, _) = event(
        &mut view,
        &mut tree,
        &node,
        &renderer,
        key(Named::PageDown, keyboard::Modifiers::empty()),
    );
    assert!(captured, "the open popup owns PageDown");
    assert_eq!(
        geometry(&mut view, &mut tree, &node, &renderer).offset.y,
        0.0
    );
    event(
        &mut view,
        &mut tree,
        &node,
        &renderer,
        key(Named::Escape, keyboard::Modifiers::empty()),
    );
    focus_role(
        &mut view,
        &mut tree,
        &node,
        &renderer,
        accessibility::Role::RadioButton,
    );
    let (captured, messages) = event(
        &mut view,
        &mut tree,
        &node,
        &renderer,
        key(Named::End, keyboard::Modifiers::empty()),
    );
    assert!(captured);
    assert!(
        messages
            .iter()
            .any(|message| matches!(message, Message::Appearance(crate::gui::Appearance::Dark))),
        "radio End selects its last choice"
    );
    assert_eq!(
        geometry(&mut view, &mut tree, &node, &renderer).offset.y,
        0.0
    );
}

#[test]
fn sheet_scroll_ignores_modified_keys_and_inactive_or_obscured_windows() {
    use keyboard::key::Named;
    let renderer = crate::gui::destinations::layout_tests::renderer();
    let app = app(true);
    let mut tree = Tree::empty();
    let make = |blocked| {
        keyboard_history::background(app.sheets(space().into(), Palette::of(false)), blocked)
    };
    {
        let mut view = make(false);
        tree.diff(view.as_widget());
        let node = view.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(480.0, 400.0)),
        );
        focus(&mut view, &mut tree, &node, &renderer, "sheet-close");
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
                key(Named::PageDown, modifiers),
            );
            assert_eq!(
                geometry(&mut view, &mut tree, &node, &renderer).offset.y,
                0.0
            );
        }
        event(
            &mut view,
            &mut tree,
            &node,
            &renderer,
            iced::Event::Window(window::Event::Unfocused),
        );
        event(
            &mut view,
            &mut tree,
            &node,
            &renderer,
            key(Named::PageDown, keyboard::Modifiers::empty()),
        );
        assert_eq!(
            geometry(&mut view, &mut tree, &node, &renderer).offset.y,
            0.0
        );
        event(
            &mut view,
            &mut tree,
            &node,
            &renderer,
            iced::Event::Window(window::Event::Focused),
        );
    }
    {
        let mut view = make(true);
        tree.diff(view.as_widget());
        let node = view.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(480.0, 400.0)),
        );
        event(
            &mut view,
            &mut tree,
            &node,
            &renderer,
            key(Named::End, keyboard::Modifiers::empty()),
        );
    }
    let mut view = make(false);
    tree.diff(view.as_widget());
    let node = view.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, Size::new(480.0, 400.0)),
    );
    assert_eq!(
        geometry(&mut view, &mut tree, &node, &renderer).offset.y,
        0.0,
        "hidden parent body did not scroll"
    );
    event(
        &mut view,
        &mut tree,
        &node,
        &renderer,
        key(Named::PageDown, keyboard::Modifiers::empty()),
    );
    assert!(
        geometry(&mut view, &mut tree, &node, &renderer).offset.y > 0.0,
        "the active sheet scrolls again"
    );
}
