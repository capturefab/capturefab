//! Removal confirmation uses the real manager, mapper and keyboard guard.
//! Activation never reaches a storage write: only cancellation is exercised.
use super::*;
use crate::gui::{Message as GuiMessage, accessibility, keyboard_history};
use iced::advanced::{
    Layout, Shell, layout,
    renderer::Headless,
    widget::{Id, Operation, Tree, operation},
};
use iced::{Event, Rectangle, Size, keyboard, mouse};

fn view(picker: &Picker) -> Element<'_, GuiMessage> {
    keyboard_history::guard(
        accessibility::mapped(
            picker.manager(Palette::of(false), 640.0, 0, Instant::now(), None, None),
            GuiMessage::CapturePicker,
        ),
        None,
        false,
    )
}

fn layout(
    view: &mut Element<'_, GuiMessage>,
    tree: &mut Tree,
    renderer: &iced::Renderer,
) -> layout::Node {
    tree.diff(view.as_widget());
    view.as_widget_mut().layout(
        tree,
        renderer,
        &layout::Limits::new(Size::ZERO, Size::new(560.0, 640.0)),
    )
}

fn apply(
    view: &mut Element<'_, GuiMessage>,
    tree: &mut Tree,
    node: &layout::Node,
    renderer: &iced::Renderer,
    mut operation: impl Operation,
) {
    view.as_widget_mut()
        .operate(tree, Layout::new(node), renderer, &mut operation);
}

fn key(named: keyboard::key::Named, released: bool) -> Event {
    let key = keyboard::Key::Named(named);
    let physical_key =
        keyboard::key::Physical::Unidentified(keyboard::key::NativeCode::Unidentified);
    if released {
        Event::Keyboard(keyboard::Event::KeyReleased {
            key: key.clone(),
            modified_key: key,
            physical_key,
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
        })
    } else {
        Event::Keyboard(keyboard::Event::KeyPressed {
            key: key.clone(),
            modified_key: key,
            physical_key,
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: None,
            repeat: false,
        })
    }
}

fn event(
    view: &mut Element<'_, GuiMessage>,
    tree: &mut Tree,
    node: &layout::Node,
    renderer: &iced::Renderer,
    event: Event,
) -> Vec<GuiMessage> {
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

#[derive(Default)]
struct Trigger(Option<Id>);
impl Operation for Trigger {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        operate(self);
    }
    fn custom(&mut self, _: Option<&Id>, _: Rectangle, state: &mut dyn std::any::Any) {
        if let Some(control) = state.downcast_ref::<accessibility::Control<GuiMessage>>()
            && control.node.label() == Some("Remove destination archive")
        {
            self.0 = Some(control.focus_id.clone());
        }
    }
}

#[test]
fn confirmation_orders_actions_and_cancels_without_losing_row_focus_or_scroll() {
    let renderer = iced::futures::executor::block_on(iced::Renderer::new(
        iced::Font::default(),
        iced::Pixels(16.0),
        Some("tiny-skia"),
    ))
    .unwrap();
    for cancellation in [
        keyboard::key::Named::Enter,
        keyboard::key::Named::Space,
        keyboard::key::Named::Escape,
    ] {
        let mut picker = Picker::new(false, false);
        picker.saved = vec![Destination {
            name: "archive".into(),
            target: Target::Folder {
                path: "/tmp/archive".into(),
            },
        }];
        picker.manager_open = true;
        picker.scrolled = true;
        let mut output = "/tmp/capture.png".into();
        let mut tree = Tree::empty();
        let opened = {
            let mut view = view(&picker);
            let node = layout(&mut view, &mut tree, &renderer);
            let mut trigger = Trigger::default();
            view.as_widget_mut()
                .operate(&mut tree, Layout::new(&node), &renderer, &mut trigger);
            apply(
                &mut view,
                &mut tree,
                &node,
                &renderer,
                operation::focusable::focus::<()>(trigger.0.unwrap()),
            );
            event(
                &mut view,
                &mut tree,
                &node,
                &renderer,
                key(keyboard::key::Named::Enter, false),
            )
        };
        assert_eq!(opened.len(), 1);
        let GuiMessage::CapturePicker(Message::ConfirmRemove(Some(name))) = &opened[0] else {
            panic!("the row first requests confirmation");
        };
        picker.update(Message::ConfirmRemove(Some(name.clone())), &mut output);
        assert_eq!(picker.take_focus(), Some(REMOVE_CANCEL));
        assert!(
            !picker.take_to_top(),
            "confirmation preserves the list's scroll"
        );
        let cancelled = {
            let mut view = view(&picker);
            let node = layout(&mut view, &mut tree, &renderer);
            let (snapshot, _, _) = accessibility::snapshot_widget(
                view.as_widget_mut(),
                &mut tree,
                Layout::new(&node),
                &renderer,
            );
            let cancel = snapshot
                .nodes
                .iter()
                .find(|(_, node)| node.label() == Some("Cancel removal of archive"))
                .unwrap()
                .1
                .bounds()
                .unwrap();
            let remove = snapshot
                .nodes
                .iter()
                .find(|(_, node)| node.label() == Some("Remove destination archive"))
                .unwrap()
                .1
                .bounds()
                .unwrap();
            if cfg!(target_os = "windows") {
                assert!(remove.x1 < cancel.x0);
            } else {
                assert!(cancel.x1 < remove.x0);
            }
            apply(
                &mut view,
                &mut tree,
                &node,
                &renderer,
                operation::focusable::focus::<()>(Id::new(REMOVE_CANCEL)),
            );
            let mut messages = event(
                &mut view,
                &mut tree,
                &node,
                &renderer,
                key(cancellation, false),
            );
            if cancellation == keyboard::key::Named::Space {
                assert!(messages.is_empty(), "Space activates on release");
                messages = event(
                    &mut view,
                    &mut tree,
                    &node,
                    &renderer,
                    key(cancellation, true),
                );
            }
            messages
        };
        assert_eq!(cancelled.len(), 1);
        match &cancelled[0] {
            GuiMessage::CapturePicker(Message::ConfirmRemove(None)) => {
                picker.update(Message::ConfirmRemove(None), &mut output)
            }
            GuiMessage::Key { chord, .. } if chord.key == crate::gui::keys::Key::Escape => {
                picker.update(Message::Cancel, &mut output)
            }
            _ => panic!("cancellation must never issue Remove"),
        }
        assert!(picker.removing.is_none() && picker.manager_open && picker.scrolled);
        assert!(picker.changes.is_empty());
        assert_eq!(picker.saved.len(), 1);
        assert_eq!(output, "/tmp/capture.png");
        assert_eq!(picker.take_focus(), Some(REMOVE_TRIGGER));
        assert!(!picker.take_to_top());
        let mut view = view(&picker);
        let node = layout(&mut view, &mut tree, &renderer);
        apply(
            &mut view,
            &mut tree,
            &node,
            &renderer,
            operation::focusable::focus::<()>(Id::new(REMOVE_TRIGGER)),
        );
        let reopened = event(
            &mut view,
            &mut tree,
            &node,
            &renderer,
            key(keyboard::key::Named::Enter, false),
        );
        assert!(
            matches!(reopened.as_slice(), [GuiMessage::CapturePicker(Message::ConfirmRemove(Some(name)))] if name == "archive")
        );
    }
}
