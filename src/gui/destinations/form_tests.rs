//! Frozen saves retain the complete readable form while refusing mutation.
use super::*;
use crate::gui::accessibility::{Node, NodeId, Role, mapped, snapshot_widget};
use iced::Size;

fn editor(source: Option<Source>) -> Editor {
    let mut editor = match source {
        None => Editor::new_folder("line-1".into(), PathBuf::from("/tmp/captures")),
        Some(source) => {
            let mut editor = Editor::new_bucket();
            editor.source = source;
            editor.provider = 4;
            editor.endpoint = "https://storage.example.com".into();
            editor.bucket = "line-1".into();
            editor.access_key_id = "AKIA".into();
            editor.secret = "test-secret".into();
            editor.profile = "camera-profile".into();
            editor.prefix = "cameras/line-1/".into();
            editor.advanced = true;
            editor
        }
    };
    editor.name = "line-1".into();
    editor.name_touched = true;
    editor
}
use iced::advanced::{
    Layout, layout,
    renderer::Headless,
    widget::{Operation, Tree, operation},
};

fn controls(nodes: Vec<(NodeId, Node)>) -> BTreeMap<NodeId, Node> {
    nodes
        .into_iter()
        .filter(|(_, node)| {
            matches!(
                node.role(),
                Role::Button
                    | Role::CheckBox
                    | Role::ComboBox
                    | Role::TextInput
                    | Role::PasswordInput
            )
        })
        .collect()
}

fn inspect(
    picker: &Picker,
    tree: &mut Tree,
    renderer: &iced::Renderer,
) -> (BTreeMap<NodeId, Node>, operation::focusable::Count) {
    let mut view = mapped(
        picker.manager(Palette::of(false), 640.0, 0, Instant::now(), None, None),
        crate::gui::Message::CapturePicker,
    );
    tree.diff(view.as_widget());
    let node = view.as_widget_mut().layout(
        tree,
        renderer,
        &layout::Limits::new(Size::ZERO, Size::new(560.0, 640.0)),
    );
    let (snapshot, _, _) =
        snapshot_widget(view.as_widget_mut(), tree, Layout::new(&node), renderer);
    let mut count = operation::focusable::count();
    view.as_widget_mut().operate(
        tree,
        Layout::new(&node),
        renderer,
        &mut operation::black_box(&mut count),
    );
    let operation::Outcome::Some(count) = count.finish() else {
        panic!("the real form exposes its traversal count");
    };
    (controls(snapshot.nodes), count)
}

#[test]
fn saving_freezes_all_destination_edits_but_keeps_their_values_readable() {
    let renderer = iced::futures::executor::block_on(iced::Renderer::new(
        iced::Font::default(),
        iced::Pixels(16.0),
        Some("tiny-skia"),
    ))
    .expect("the compiled software renderer is available");
    for source in [
        None,
        Some(Source::Keychain),
        Some(Source::Profile),
        Some(Source::Environment),
    ] {
        let mut picker = Picker::new(false, false);
        picker.manager_open = true;
        picker.editor = Some(editor(source));
        let mut tree = Tree::empty();
        let (before, before_focus) = inspect(&picker, &mut tree, &renderer);
        assert!(before.values().any(|node| node.role() == Role::TextInput));
        assert!(before_focus.total > 0);
        let save = before
            .values()
            .find(|node| node.label() == Some("Save"))
            .unwrap()
            .bounds()
            .unwrap();
        let cancel = before
            .values()
            .find(|node| node.label() == Some("Cancel"))
            .unwrap()
            .bounds()
            .unwrap();
        if cfg!(target_os = "windows") {
            assert!(save.x1 < cancel.x0, "Windows puts Save before Cancel");
        } else {
            assert!(
                cancel.x1 < save.x0,
                "macOS and GNOME put Cancel before Save"
            );
        }

        let (_sender, receiver) = channel();
        let editor = picker.editor.as_mut().unwrap();
        editor.saving = Some((editor.destination().unwrap(), receiver));
        let (pending, pending_focus) = inspect(&picker, &mut tree, &renderer);
        assert_eq!(pending.len(), before.len(), "all controls remain visible");
        for (id, node) in &pending {
            let previous = &before[id];
            assert_eq!(node.label(), previous.label());
            assert_eq!(
                node.value(),
                previous.value(),
                "the frozen draft remains readable"
            );
            assert_eq!(node.toggled(), previous.toggled());
            assert!(
                node.is_disabled(),
                "{} is unavailable during save",
                node.label().unwrap()
            );
            for action in [
                accesskit::Action::Click,
                accesskit::Action::Focus,
                accesskit::Action::SetValue,
                accesskit::Action::ReplaceSelectedText,
                accesskit::Action::SetTextSelection,
                accesskit::Action::Expand,
            ] {
                assert!(
                    !node.supports_action(action),
                    "a disabled control offers no {action:?}"
                );
            }
        }
        assert_eq!(
            pending_focus.total,
            usize::from(source.is_some()),
            "only S3's Advanced disclosure remains traversable for reading"
        );
        if source.is_some() {
            // This is a permitted local navigation action, even during save.
            let mut output = String::new();
            picker.update(Message::Advanced(false), &mut output);
            assert!(!picker.editor.as_ref().unwrap().advanced);
            picker.update(Message::Advanced(true), &mut output);
        }

        picker.editor.as_mut().unwrap().saving = None;
        let (after, after_focus) = inspect(&picker, &mut tree, &renderer);
        assert_eq!(after.len(), before.len());
        assert_eq!(after_focus.total, before_focus.total);
        for (id, node) in after {
            assert_eq!(node.is_disabled(), before[&id].is_disabled());
            assert_eq!(node.value(), before[&id].value());
        }
    }
}

#[test]
fn return_from_each_actual_single_line_field_requests_save_once() {
    use crate::gui::{Message as GuiMessage, accessibility::Control};
    use iced::advanced::{Shell, widget::Id};
    use iced::{Event, Rectangle, keyboard, mouse};
    use std::any::Any;
    #[derive(Default)]
    struct Fields(Vec<Id>);
    impl Operation for Fields {
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
            operate(self);
        }
        fn custom(&mut self, _: Option<&Id>, _: Rectangle, state: &mut dyn Any) {
            if let Some(control) = state.downcast_ref::<Control<GuiMessage>>()
                && matches!(control.node.role(), Role::TextInput | Role::PasswordInput)
                && !control.node.is_disabled()
            {
                self.0.push(control.focus_id.clone());
            }
        }
    }
    let renderer = iced::futures::executor::block_on(iced::Renderer::new(
        iced::Font::default(),
        iced::Pixels(16.0),
        Some("tiny-skia"),
    ))
    .unwrap();
    for source in [
        None,
        Some(Source::Keychain),
        Some(Source::Profile),
        Some(Source::Environment),
    ] {
        let mut picker = Picker::new(false, false);
        picker.editor = Some(editor(source));
        picker.manager_open = true;
        let mut view = mapped(
            picker.manager(Palette::of(false), 640.0, 0, Instant::now(), None, None),
            GuiMessage::CapturePicker,
        );
        let mut tree = Tree::new(view.as_widget());
        let node = view.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(560.0, 640.0)),
        );
        let mut fields = Fields::default();
        view.as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut fields);
        assert!(!fields.0.is_empty());
        for id in fields.0 {
            view.as_widget_mut().operate(
                &mut tree,
                Layout::new(&node),
                &renderer,
                &mut operation::focusable::focus::<()>(id),
            );
            for repeat in [false, true] {
                let enter = Event::Keyboard(keyboard::Event::KeyPressed {
                    key: keyboard::Key::Named(keyboard::key::Named::Enter),
                    modified_key: keyboard::Key::Named(keyboard::key::Named::Enter),
                    physical_key: keyboard::key::Physical::Unidentified(
                        keyboard::key::NativeCode::Unidentified,
                    ),
                    location: keyboard::Location::Standard,
                    modifiers: keyboard::Modifiers::empty(),
                    text: None,
                    repeat,
                });
                let mut messages = Vec::new();
                view.as_widget_mut().update(
                    &mut tree,
                    &enter,
                    Layout::new(&node),
                    mouse::Cursor::Unavailable,
                    &renderer,
                    &mut iced::advanced::clipboard::Null,
                    &mut Shell::new(&mut messages),
                    &Rectangle::with_size(node.size()),
                );
                if repeat {
                    assert!(messages.is_empty());
                } else {
                    assert!(matches!(
                        messages.as_slice(),
                        [GuiMessage::CapturePicker(Message::Save)]
                    ));
                }
            }
        }
    }
}
