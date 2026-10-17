use super::*;
use crate::gui::{Tab, Workbench, destinations, style};
use iced::advanced::widget::{Tree, tree::Tag};
use iced::advanced::{Layout, layout};
use iced::widget::{scrollable, space};

fn renderer() -> iced::Renderer {
    destinations::layout_tests::renderer()
}

fn apply(
    view: &mut Element<'_, Message>,
    tree: &mut Tree,
    node: &layout::Node,
    renderer: &iced::Renderer,
    operation: impl Operation<Message> + 'static,
) {
    let mut operation: Box<dyn Operation<Message>> = Box::new(operation);
    loop {
        let mut erased = operation::black_box(operation.as_mut());
        view.as_widget_mut()
            .operate(tree, Layout::new(node), renderer, &mut erased);
        drop(erased);
        match operation.finish() {
            Outcome::Chain(next) => operation = next,
            _ => break,
        }
    }
}

fn snapshot(
    view: &mut Element<'_, Message>,
    tree: &mut Tree,
    node: &layout::Node,
    renderer: &iced::Renderer,
) -> TreeUpdate {
    super::super::snapshot_widget(view.as_widget_mut(), tree, Layout::new(node), renderer).0
}

fn named<'a>(tree: &'a TreeUpdate, name: &str, role: Role) -> (NodeId, &'a Node) {
    let (id, node) = tree
        .nodes
        .iter()
        .find(|(_, node)| node.role() == role && node.label() == Some(name))
        .unwrap_or_else(|| {
            panic!(
                "missing {role:?} {name}; labels: {:?}",
                tree.nodes
                    .iter()
                    .map(|(_, node)| (node.role(), node.label()))
                    .collect::<Vec<_>>()
            )
        });
    (*id, node)
}

#[derive(Default)]
struct Raw {
    scrolls: Vec<(Rectangle, Vector)>,
    controls: HashMap<String, Rectangle>,
    windows: HashMap<String, Rectangle>,
    focusable_states: Vec<(Option<Id>, bool)>,
}
impl Operation<Message> for Raw {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation<Message>)) {
        operate(self);
    }
    fn focusable(&mut self, id: Option<&Id>, _: Rectangle, state: &mut dyn operation::Focusable) {
        self.focusable_states
            .push((id.cloned(), state.is_focused()));
    }
    fn scrollable(
        &mut self,
        _: Option<&Id>,
        bounds: Rectangle,
        _: Rectangle,
        translation: Vector,
        _: &mut dyn Scrollable,
    ) {
        self.scrolls.push((bounds, translation));
    }
    fn custom(&mut self, _: Option<&Id>, bounds: Rectangle, state: &mut dyn std::any::Any) {
        if let Some(control) = state.downcast_ref::<Control<Message>>()
            && let Some(name) = control.node.label()
        {
            self.controls.insert(name.into(), bounds);
            if let Some(viewport) = control.window_coordinates {
                self.windows.insert(name.into(), viewport);
            }
        }
    }
}

fn raw(
    view: &mut Element<'_, Message>,
    tree: &mut Tree,
    node: &layout::Node,
    renderer: &iced::Renderer,
) -> Raw {
    let mut raw = Raw::default();
    let mut erased = operation::black_box(&mut raw);
    view.as_widget_mut()
        .operate(tree, Layout::new(node), renderer, &mut erased);
    drop(erased);
    raw
}

#[test]
fn actual_scrolled_inspector_bounds_follow_visual_translation_and_native_focus_reveals_fields() {
    let renderer = renderer();
    let mut app = Workbench::new(
        crate::session::SessionHandle::new(),
        "scroll-test".into(),
        true,
        None,
    );
    app.snapshot.connected = Some(crate::transport::simulator::info());
    app.snapshot.active_camera = Some("sim:0".into());
    app.tab = Tab::Capture;
    app.reveal_schedule();
    app.storage_open = true;
    app.retention_enabled = true;
    let mut view = app.inspector(style::Palette::of(false));
    let mut tree = Tree::new(view.as_widget());
    let node = view.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, Size::new(316.0, 280.0)),
    );
    let before = snapshot(&mut view, &mut tree, &node, &renderer);
    let (target, field) = named(&before, "Maximum file age", Role::TextInput);
    assert_eq!(field.bounds().unwrap().height(), 0.0);
    assert!(!field.is_hidden());
    assert!(field.supports_action(Action::Focus));
    assert!(field.supports_action(Action::ScrollIntoView));
    let window = window::Id::unique();
    apply(
        &mut view,
        &mut tree,
        &node,
        &renderer,
        Locate::node(target, window, true),
    );
    let raw = raw(&mut view, &mut tree, &node, &renderer);
    let (viewport, translation) = raw.scrolls[0];
    assert!(translation.y > 0.0);
    let after = snapshot(&mut view, &mut tree, &node, &renderer);
    let (id, field) = named(&after, "Maximum file age", Role::TextInput);
    assert_eq!(id, target);
    assert_eq!(after.focus, target);
    let expected = clipped(raw.controls["Maximum file age"] - translation, viewport);
    assert_eq!(field.bounds(), Some(rect(expected, 1.0)));
    assert!(
        expected.height > 15.0
            && expected.y >= viewport.y
            && expected.y + expected.height <= viewport.y + viewport.height + 0.1
    );
    assert_eq!(
        named(&before, "Capture", Role::Tab).1.bounds(),
        named(&after, "Capture", Role::Tab).1.bounds()
    );
}

#[test]
fn actual_mapped_destination_editor_uses_scrolled_bounds_and_reveals_native_focus() {
    let renderer = renderer();
    let mut picker = destinations::Picker::new(false, false);
    let mut output = "capture.png".into();
    picker.update(destinations::Message::AddBucket, &mut output);
    picker.update(destinations::Message::Advanced(true), &mut output);
    let mut view = super::super::mapped(
        picker.manager(
            style::Palette::of(false),
            310.0,
            0,
            std::time::Instant::now(),
            None,
            None,
        ),
        Message::CapturePicker,
    );
    let mut tree = Tree::new(view.as_widget());
    let node = view.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, Size::new(560.0, 310.0)),
    );
    let before = snapshot(&mut view, &mut tree, &node, &renderer);
    let (target, field) = named(&before, "S3 secret access key", Role::PasswordInput);
    assert_eq!(field.bounds().unwrap().height(), 0.0);
    apply(
        &mut view,
        &mut tree,
        &node,
        &renderer,
        Locate::node(target, window::Id::unique(), true),
    );
    let geometry = raw(&mut view, &mut tree, &node, &renderer);
    let (viewport, translation) = geometry.scrolls[0];
    let after = snapshot(&mut view, &mut tree, &node, &renderer);
    let (id, field) = named(&after, "S3 secret access key", Role::PasswordInput);
    assert_eq!(id, target);
    assert_eq!(after.focus, target);
    assert_eq!(
        field.bounds(),
        Some(rect(
            clipped(
                geometry.controls["S3 secret access key"] - translation,
                viewport
            ),
            1.0
        ))
    );
    assert!(field.bounds().unwrap().height() > 15.0);
    assert_eq!(
        named(&before, "Close New S3 bucket", Role::Button)
            .1
            .bounds(),
        named(&after, "Close New S3 bucket", Role::Button)
            .1
            .bounds()
    );
}

#[test]
fn nested_real_scrollables_clip_and_reveal_inside_out_without_resetting_selection() {
    let renderer = renderer();
    let mut view: Element<'_, Message> = scrollable(iced::widget::column![
        space().height(120),
        scrollable(iced::widget::column![
            space().height(180.15),
            crate::gui::text_input::text_input("Nested value", "abcd")
                .id("nested-value")
                .on_input(Message::Address),
            space().height(180),
        ])
        .id("inner-scroll")
        .height(120),
        space().height(180),
    ])
    .id("outer-scroll")
    .height(200)
    .into();
    let mut tree = Tree::new(view.as_widget());
    let node = view.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, Size::new(300.0, 200.0)),
    );
    let before = snapshot(&mut view, &mut tree, &node, &renderer);
    let target = named(&before, "Nested value", Role::TextInput).0;
    assert_eq!(
        named(&before, "Nested value", Role::TextInput)
            .1
            .bounds()
            .unwrap()
            .height(),
        0.0
    );
    let window = window::Id::unique();
    apply(
        &mut view,
        &mut tree,
        &node,
        &renderer,
        Locate::node(target, window, true),
    );
    let geometry = raw(&mut view, &mut tree, &node, &renderer);
    assert!(geometry.scrolls.iter().all(|(_, offset)| offset.y > 0.0));
    let after = snapshot(&mut view, &mut tree, &node, &renderer);
    let field = named(&after, "Nested value", Role::TextInput).1;
    let outer = geometry.scrolls[0];
    let inner = geometry.scrolls[1];
    let expected = clipped(
        clipped(
            geometry.controls["Nested value"] - inner.1 - outer.1,
            inner.0 - outer.1,
        ),
        outer.0,
    );
    assert_eq!(field.bounds(), Some(rect(expected, 1.0)));
    assert!(field.bounds().unwrap().height() > 15.0);
    assert!(
        (field.bounds().unwrap().height() - f64::from(geometry.controls["Nested value"].height))
            .abs()
            < 0.001,
        "integer scroll rounding must reveal the whole fractional-height editor"
    );
    apply(
        &mut view,
        &mut tree,
        &node,
        &renderer,
        operation::text_input::select_range(Id::new("nested-value"), 1, 3),
    );
    for id in ["outer-scroll", "inner-scroll"] {
        apply(
            &mut view,
            &mut tree,
            &node,
            &renderer,
            operation::scrollable::snap_to(
                Id::new(id),
                operation::scrollable::RelativeOffset {
                    x: Some(0.0),
                    y: Some(0.0),
                },
            ),
        );
    }
    apply(
        &mut view,
        &mut tree,
        &node,
        &renderer,
        Locate::focused(window),
    );
    type Editor = iced::widget::text_input::State<
        <iced::Renderer as iced::advanced::text::Renderer>::Paragraph,
    >;
    fn editor(tree: &Tree) -> Option<&Editor> {
        if tree.tag == Tag::of::<Editor>() {
            Some(tree.state.downcast_ref())
        } else {
            tree.children.iter().find_map(editor)
        }
    }
    assert_eq!(
        editor(&tree)
            .unwrap()
            .cursor()
            .state(&iced::widget::text_input::Value::new("abcd")),
        iced::widget::text_input::cursor::State::Selection { start: 1, end: 3 }
    );
    assert_eq!(
        snapshot(&mut view, &mut tree, &node, &renderer).focus,
        target
    );
}

#[test]
fn real_popup_rows_escape_the_scrolled_parent_and_keep_window_hit_bounds() {
    let renderer = renderer();
    let mut view: Element<'_, Message> = iced::widget::column![
        scrollable(iced::widget::column![
            space().height(120),
            crate::gui::pick_list::pick_list(["One", "Two", "Three"], Some("One"), |value| {
                Message::Address(value.into())
            })
            .name("Scrolled choices")
            .padding(5),
            space().height(200),
        ])
        .id("popup-parent")
        .height(100),
        space().height(150),
    ]
    .into();
    let mut tree = Tree::new(view.as_widget());
    let node = view.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, Size::new(300.0, 250.0)),
    );
    apply(
        &mut view,
        &mut tree,
        &node,
        &renderer,
        operation::scrollable::scroll_to(
            Id::new("popup-parent"),
            operation::scrollable::AbsoluteOffset {
                x: None,
                y: Some(100.0),
            },
        ),
    );
    let native = snapshot(&mut view, &mut tree, &node, &renderer);
    let target = named(&native, "Scrolled choices", Role::ComboBox).0;
    apply(
        &mut view,
        &mut tree,
        &node,
        &renderer,
        super::super::WidgetAction(
            ActionRequest {
                action: Action::Expand,
                target,
                data: None,
            },
            window::Id::unique(),
        ),
    );
    let viewport = Rectangle::new(iced::Point::ORIGIN, Size::new(300.0, 250.0));
    let mut overlay = view
        .as_widget_mut()
        .overlay(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &viewport,
            Vector::ZERO,
        )
        .unwrap();
    overlay.as_overlay_mut().layout(&renderer, viewport.size());
    drop(overlay);
    let geometry = raw(&mut view, &mut tree, &node, &renderer);
    assert!(geometry.scrolls[0].1.y > 0.0);
    let native = snapshot(&mut view, &mut tree, &node, &renderer);
    for name in ["One", "Two", "Three"] {
        let option = named(&native, name, Role::ListBoxOption).1;
        let expected = clipped(geometry.controls[name], geometry.windows[name]);
        assert_eq!(
            option.bounds(),
            Some(rect(expected, 1.0)),
            "popup rows use their actual window coordinates once"
        );
    }
    let last = named(&native, "Three", Role::ListBoxOption)
        .1
        .bounds()
        .unwrap();
    assert!(
        last.height() > 0.0
            && last.y1 > f64::from(geometry.scrolls[0].0.y + geometry.scrolls[0].0.height)
    );
}

#[test]
fn focused_reveal_is_scoped_to_its_native_window_and_preserves_other_window_focus() {
    let renderer = renderer();
    struct Surface {
        view: Element<'static, Message>,
        tree: Tree,
        node: layout::Node,
    }
    let first = window::Id::unique();
    let second = window::Id::unique();
    let mut surfaces: Vec<_> = [(first, "First value"), (second, "Second value")]
        .into_iter()
        .map(|(id, label)| {
            let content = scrollable(iced::widget::column![
                space().height(150),
                crate::gui::text_input::text_input(label, "abcd").on_input(Message::Address),
                space().height(150),
            ])
            .height(120)
            .into();
            let mut view = super::super::wrap(
                content,
                Bridge::default(),
                id,
                label.into(),
                1.0,
                Message::AccessibilityChanged,
            );
            let mut tree = Tree::new(view.as_widget());
            let node = view.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, Size::new(300.0, 120.0)),
            );
            type Editor = iced::widget::text_input::State<
                <iced::Renderer as iced::advanced::text::Renderer>::Paragraph,
            >;
            fn focus(tree: &mut Tree) {
                if tree.tag == Tag::of::<Editor>() {
                    tree.state.downcast_mut::<Editor>().focus();
                }
                for child in &mut tree.children {
                    focus(child);
                }
            }
            focus(&mut tree);
            Surface { view, tree, node }
        })
        .collect();
    let mut operation: Box<dyn Operation<Message>> = Box::new(Locate::focused(first));
    loop {
        for surface in &mut surfaces {
            let mut erased = operation::black_box(operation.as_mut());
            surface.view.as_widget_mut().operate(
                &mut surface.tree,
                Layout::new(&surface.node),
                &renderer,
                &mut erased,
            );
        }
        match operation.finish() {
            Outcome::Chain(next) => operation = next,
            _ => break,
        }
    }
    for (index, surface) in surfaces.iter_mut().enumerate() {
        let geometry = raw(
            &mut surface.view,
            &mut surface.tree,
            &surface.node,
            &renderer,
        );
        assert_eq!(
            geometry.scrolls[0].1.y > 0.0,
            index == 0,
            "only the requested native window scrolls"
        );
        let native = snapshot(
            &mut surface.view,
            &mut surface.tree,
            &surface.node,
            &renderer,
        );
        let (id, _) = named(
            &native,
            if index == 0 {
                "First value"
            } else {
                "Second value"
            },
            Role::TextInput,
        );
        assert_eq!(
            native.focus, id,
            "both windows retain their existing editor focus"
        );
    }
}

#[test]
fn native_focus_scopes_duplicate_explicit_ids_and_only_activates_current_enabled_routes() {
    let renderer = renderer();
    let bridge = Bridge::default();
    struct Surface {
        id: window::Id,
        title: &'static str,
        view: Element<'static, Message>,
        tree: Tree,
        node: layout::Node,
    }
    let view = |id, title: &'static str, enabled: bool| {
        super::super::wrap(
            iced::widget::column![
                crate::gui::text_input::text_input(title, "abcd")
                    .id("shared-editor")
                    .on_input(Message::Address),
                crate::gui::button::button(iced::widget::text("Close"))
                    .id("sheet-close")
                    .name(format!("Close {title}"))
                    .on_press_maybe(enabled.then_some(Message::Help(false))),
                super::super::label(
                    iced::widget::text("Read-only explanation").into(),
                    "Read-only explanation".into(),
                    Role::Group,
                ),
            ]
            .into(),
            bridge.clone(),
            id,
            title.into(),
            1.0,
            Message::AccessibilityChanged,
        )
    };
    let mut surfaces: Vec<_> = ["Main", "Settings"]
        .into_iter()
        .map(|title| {
            let id = window::Id::unique();
            let mut view = view(id, title, true);
            let mut tree = Tree::new(view.as_widget());
            let node = view.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, Size::new(300.0, 120.0)),
            );
            let native = snapshot(&mut view, &mut tree, &node, &renderer);
            let editor = named(&native, title, Role::TextInput).0;
            apply(
                &mut view,
                &mut tree,
                &node,
                &renderer,
                Locate::node(editor, id, true),
            );
            Surface {
                id,
                title,
                view,
                tree,
                node,
            }
        })
        .collect();
    struct FocusCallback;
    impl Operation<Message> for FocusCallback {
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation<Message>)) {
            operate(self);
        }
        fn custom(&mut self, _: Option<&Id>, _: Rectangle, state: &mut dyn std::any::Any) {
            if let Some(control) = state.downcast_mut::<Control<Message>>()
                && control.node.label() == Some("Close Main")
            {
                control.on_focus = Some(Message::Address("native focus callback".into()));
            }
        }
    }
    let surface = &mut surfaces[0];
    apply(
        &mut surface.view,
        &mut surface.tree,
        &surface.node,
        &renderer,
        FocusCallback,
    );
    let collect = |surface: &mut Surface| {
        let mut collector = super::super::Collector::new(1.0);
        surface.view.as_widget_mut().operate(
            &mut surface.tree,
            Layout::new(&surface.node),
            &renderer,
            &mut collector,
        );
        let (native, routes) = collector.finish(surface.title, surface.node.bounds());
        bridge.store(surface.id, native.clone(), routes);
        native
    };
    let request = |window, target| super::super::Request {
        window,
        replay: None,
        action: ActionRequest {
            action: Action::Focus,
            target,
            data: None,
        },
    };
    for requested in 0..2 {
        let native = collect(&mut surfaces[requested]);
        let target = named(
            &native,
            &format!("Close {}", surfaces[requested].title),
            Role::Button,
        )
        .0;
        let window = surfaces[requested].id;
        let task = bridge.dispatch(request(window, target));
        let mut actions = iced_runtime::task::into_stream(task).unwrap();
        let Some(iced_runtime::Action::Window(iced_runtime::window::Action::GetMode(id, reply))) =
            iced::futures::executor::block_on(actions.next())
        else {
            panic!("native Focus must inspect its owning window before restoring it");
        };
        assert_eq!(id, window);
        reply.send(window::Mode::Windowed).unwrap();
        assert!(
            matches!(
                iced::futures::executor::block_on(actions.next()),
                Some(iced_runtime::Action::Window(iced_runtime::window::Action::Minimize(id, false))) if id == window
            ),
            "native Focus first restores only the requested window"
        );
        assert!(
            matches!(
                iced::futures::executor::block_on(actions.next()),
                Some(iced_runtime::Action::Window(iced_runtime::window::Action::GainFocus(id))) if id == window
            ),
            "native Focus activates the restored window before changing its widget focus"
        );
        let Some(iced_runtime::Action::Widget(mut operation)) =
            iced::futures::executor::block_on(actions.next())
        else {
            panic!("native Focus must next run the actual scoped widget operation");
        };
        loop {
            for surface in &mut surfaces {
                surface.view.as_widget_mut().operate(
                    &mut surface.tree,
                    Layout::new(&surface.node),
                    &renderer,
                    operation.as_mut(),
                );
            }
            match operation.finish() {
                Outcome::Chain(next) => operation = next,
                _ => break,
            }
        }
        drop(operation);
        let outputs = iced::futures::executor::block_on(actions.collect::<Vec<_>>());
        assert!(
            matches!(outputs.first(), Some(iced_runtime::Action::Output(Message::AccessibilityChanged(id))) if *id == window)
        );
        if requested == 0 {
            assert!(
                matches!(outputs.last(), Some(iced_runtime::Action::Output(Message::Address(value))) if value == "native focus callback"),
                "the focus callback follows real widget focus and its semantic refresh"
            );
        }
        for (index, surface) in surfaces.iter_mut().enumerate() {
            let native = collect(surface);
            let expected = if index <= requested {
                named(&native, &format!("Close {}", surface.title), Role::Button).0
            } else {
                named(&native, surface.title, Role::TextInput).0
            };
            assert_eq!(
                native.focus, expected,
                "a duplicated explicit ID must not clear or steal another window's retained focus"
            );
            let focused = raw(
                &mut surface.view,
                &mut surface.tree,
                &surface.node,
                &renderer,
            )
            .focusable_states
            .into_iter()
            .find(|(id, _)| id.as_ref() == Some(&Id::new("sheet-close")))
            .unwrap()
            .1;
            assert_eq!(
                focused,
                index <= requested,
                "the real button state agrees with the native focus, including the callback-bearing control"
            );
        }
    }
    let native = collect(&mut surfaces[0]);
    let target = named(&native, "Close Main", Role::Button).0;
    let static_text = named(&native, "Read-only explanation", Role::Group).0;
    assert_eq!(
        bridge
            .dispatch(request(surfaces[0].id, static_text))
            .units(),
        0
    );
    assert_eq!(
        bridge
            .dispatch(request(surfaces[0].id, NodeId(u64::MAX)))
            .units(),
        0
    );
    bridge.invalidate(surfaces[0].id);
    assert_eq!(
        bridge.dispatch(request(surfaces[0].id, target)).units(),
        0,
        "invalidated controls cannot activate their native window"
    );

    let surface = &mut surfaces[1];
    surface.view = view(surface.id, surface.title, false);
    surface.tree.diff(surface.view.as_widget());
    surface.node = surface.view.as_widget_mut().layout(
        &mut surface.tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, Size::new(300.0, 120.0)),
    );
    let native = collect(surface);
    let target = named(&native, "Close Settings", Role::Button).0;
    assert!(
        named(&native, "Close Settings", Role::Button)
            .1
            .is_disabled()
    );
    assert_eq!(
        bridge.dispatch(request(surface.id, target)).units(),
        0,
        "a disabled native control cannot activate its window"
    );
    let _ = bridge.detach(surface.id);
    assert_eq!(
        bridge.dispatch(request(surface.id, target)).units(),
        0,
        "closed-window routes cannot activate a replacement window"
    );
}

#[test]
fn widget_focus_and_reveal_only_change_the_requested_window_with_duplicate_ids() {
    use iced::futures::StreamExt;
    let renderer = renderer();
    struct Surface {
        id: window::Id,
        view: Element<'static, Message>,
        tree: Tree,
        node: layout::Node,
    }
    let mut surfaces: Vec<_> = [true, false]
        .into_iter()
        .map(|main| {
            let id = window::Id::unique();
            let content: Element<'static, Message> = if main {
                iced::widget::column![
                    crate::gui::text_input::text_input("Main editor", "abcdef")
                        .name("Main editor")
                        .id("main-editor")
                        .on_input(Message::Address),
                    space().height(400),
                    crate::gui::button::button(iced::widget::text("History data"))
                        .name("History data")
                        .id("history-target")
                        .on_press(Message::Help(false)),
                ]
                .into()
            } else {
                crate::gui::text_input::text_input("Settings editor", "a🇨🇦e\u{301}z")
                    .name("Settings editor")
                    .id("history-target")
                    .on_input(Message::Address)
                    .into()
            };
            let mut view = super::super::wrap(
                scrollable(content).height(120).into(),
                Bridge::default(),
                id,
                if main { "Main" } else { "Settings" }.into(),
                1.0,
                Message::AccessibilityChanged,
            );
            let mut tree = Tree::new(view.as_widget());
            let node = view.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, Size::new(300.0, 120.0)),
            );
            let native = snapshot(&mut view, &mut tree, &node, &renderer);
            let editor = named(
                &native,
                if main {
                    "Main editor"
                } else {
                    "Settings editor"
                },
                Role::TextInput,
            )
            .0;
            apply(
                &mut view,
                &mut tree,
                &node,
                &renderer,
                Locate::node(editor, id, true),
            );
            apply(
                &mut view,
                &mut tree,
                &node,
                &renderer,
                operation::text_input::select_range(
                    Id::new(if main {
                        "main-editor"
                    } else {
                        "history-target"
                    }),
                    1,
                    3,
                ),
            );
            Surface {
                id,
                view,
                tree,
                node,
            }
        })
        .collect();
    let settings = &mut surfaces[1];
    let before = snapshot(
        &mut settings.view,
        &mut settings.tree,
        &settings.node,
        &renderer,
    );
    let (settings_editor, editor) = named(&before, "Settings editor", Role::TextInput);
    let selection = editor.text_selection().cloned();
    assert!(selection.is_some());
    let main = surfaces[0].id;
    let task = super::super::focus_in_window("history-target", main);
    let mut actions = iced_runtime::task::into_stream(task).unwrap();
    let mut widgets = 0;
    while let Some(action) = iced::futures::executor::block_on(actions.next()) {
        match action {
            iced_runtime::Action::Widget(mut operation) => {
                widgets += 1;
                loop {
                    for surface in &mut surfaces {
                        surface.view.as_widget_mut().operate(
                            &mut surface.tree,
                            Layout::new(&surface.node),
                            &renderer,
                            operation.as_mut(),
                        );
                    }
                    match operation.finish() {
                        Outcome::Chain(next) => operation = next,
                        _ => break,
                    }
                }
            }
            iced_runtime::Action::Output(Message::AccessibilityChanged(window)) => {
                assert_eq!(window, main)
            }
            _ => panic!("local history focus must not activate, restore, or mutate another window"),
        }
    }
    assert_eq!(widgets, 1);
    let surface = &mut surfaces[0];
    let focused = snapshot(
        &mut surface.view,
        &mut surface.tree,
        &surface.node,
        &renderer,
    );
    assert_eq!(
        focused.focus,
        named(&focused, "History data", Role::Button).0
    );
    assert!(
        raw(
            &mut surface.view,
            &mut surface.tree,
            &surface.node,
            &renderer
        )
        .scrolls[0]
            .1
            .y
            > 0.0
    );
    let settings = &mut surfaces[1];
    let after = snapshot(
        &mut settings.view,
        &mut settings.tree,
        &settings.node,
        &renderer,
    );
    assert_eq!(after.focus, settings_editor);
    let (_, editor) = named(&after, "Settings editor", Role::TextInput);
    assert_eq!(editor.text_selection(), selection.as_ref());
    assert_eq!(editor.value(), Some("a🇨🇦e\u{301}z"));
    assert_eq!(
        raw(
            &mut settings.view,
            &mut settings.tree,
            &settings.node,
            &renderer
        )
        .scrolls[0]
            .1,
        Vector::ZERO
    );
}
