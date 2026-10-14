//! A dialog's default is distinct from keyboard focus on macOS. Text entry
//! and an open dropdown retain their own Return handling on every platform.
use super::*;

pub(in crate::gui) fn default_action<'a, M: Clone + 'static>(
    content: Element<'a, M>,
    action: Option<M>,
) -> Element<'a, M> {
    Element::new(DefaultAction {
        content,
        action,
        os: Os::CURRENT,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gui::{button::Button, pick_list::PickList, text_input::TextInput};
    use iced::{
        Theme,
        widget::{Row, Space},
    };
    type Dialog = DefaultAction<'static, String, Theme, ()>;
    fn dialog(os: Os, submit: bool) -> Dialog {
        DefaultAction {
            content: Row::new()
                .push(
                    TextInput::<_, ()>::new("", "draft")
                        .id("field")
                        .on_input(|value| format!("edited:{value}"))
                        .on_submit_maybe(submit.then(|| "save".into())),
                )
                .push(
                    Button::<_, ()>::new(Space::new().width(40).height(30))
                        .id("cancel")
                        .on_press("cancel".into()),
                )
                .push(
                    PickList::<_, _, ()>::new(["One", "Two"], Some("One"), |value| {
                        format!("picked:{value}")
                    })
                    .id("choice"),
                )
                .into(),
            action: Some("save".into()),
            os,
        }
    }
    fn setup(dialog: &mut Dialog) -> (Tree, layout::Node) {
        let mut tree = Tree::new(dialog as &dyn Widget<String, Theme, ()>);
        let node = dialog.layout(
            &mut tree,
            &(),
            &layout::Limits::new(Size::ZERO, Size::new(400.0, 60.0)),
        );
        (tree, node)
    }
    fn focus(dialog: &mut Dialog, tree: &mut Tree, node: &layout::Node, id: &'static str) {
        dialog.operate(
            tree,
            Layout::new(node),
            &(),
            &mut operation::focusable::focus::<()>(Id::new(id)),
        );
    }
    fn event(
        dialog: &mut Dialog,
        tree: &mut Tree,
        node: &layout::Node,
        event: Event,
    ) -> Vec<String> {
        let mut messages = Vec::new();
        dialog.update(
            tree,
            &event,
            Layout::new(node),
            mouse::Cursor::Unavailable,
            &(),
            &mut iced::advanced::clipboard::Null,
            &mut Shell::new(&mut messages),
            &Rectangle::with_size(node.size()),
        );
        messages
    }
    fn press(named: keyboard::key::Named, repeat: bool) -> Event {
        let mut event = super::super::tests::named_press(named, keyboard::Modifiers::empty());
        if let Event::Keyboard(keyboard::Event::KeyPressed { repeat: actual, .. }) = &mut event {
            *actual = repeat;
        }
        event
    }
    #[test]
    fn dialog_defaults_respect_platform_focus_and_single_line_entry_without_duplicate_submit() {
        for os in [Os::Mac, Os::Windows, Os::Nix] {
            for submit in [false, true] {
                let mut dialog = dialog(os, submit);
                let (mut tree, node) = setup(&mut dialog);
                focus(&mut dialog, &mut tree, &node, "field");
                assert_eq!(
                    event(
                        &mut dialog,
                        &mut tree,
                        &node,
                        press(keyboard::key::Named::Enter, false)
                    ),
                    ["save"]
                );
                assert!(
                    event(
                        &mut dialog,
                        &mut tree,
                        &node,
                        press(keyboard::key::Named::Enter, true)
                    )
                    .is_empty()
                );
                focus(&mut dialog, &mut tree, &node, "cancel");
                let expected = if os == Os::Mac { "save" } else { "cancel" };
                assert_eq!(
                    event(
                        &mut dialog,
                        &mut tree,
                        &node,
                        press(keyboard::key::Named::Enter, false)
                    ),
                    [expected]
                );
                assert!(
                    event(
                        &mut dialog,
                        &mut tree,
                        &node,
                        press(keyboard::key::Named::Space, false)
                    )
                    .is_empty()
                );
                let release = Event::Keyboard(keyboard::Event::KeyReleased {
                    key: keyboard::Key::Named(keyboard::key::Named::Space),
                    modified_key: keyboard::Key::Named(keyboard::key::Named::Space),
                    physical_key: keyboard::key::Physical::Unidentified(
                        keyboard::key::NativeCode::Unidentified,
                    ),
                    location: keyboard::Location::Standard,
                    modifiers: keyboard::Modifiers::empty(),
                });
                assert_eq!(event(&mut dialog, &mut tree, &node, release), ["cancel"]);
                dialog.operate(
                    &mut tree,
                    Layout::new(&node),
                    &(),
                    &mut operation::focusable::unfocus::<()>(),
                );
                assert_eq!(
                    event(
                        &mut dialog,
                        &mut tree,
                        &node,
                        press(keyboard::key::Named::Enter, false)
                    ),
                    ["save"]
                );
            }
        }
    }
    #[test]
    fn popup_confirmation_and_ime_commit_do_not_submit_the_enclosing_dialog() {
        for os in [Os::Mac, Os::Windows, Os::Nix] {
            let mut dialog = dialog(os, true);
            let (mut tree, node) = setup(&mut dialog);
            focus(&mut dialog, &mut tree, &node, "choice");
            assert!(
                event(
                    &mut dialog,
                    &mut tree,
                    &node,
                    press(keyboard::key::Named::Space, false)
                )
                .is_empty()
            );
            assert_eq!(
                event(
                    &mut dialog,
                    &mut tree,
                    &node,
                    press(keyboard::key::Named::Enter, false)
                ),
                ["picked:One"]
            );
            focus(&mut dialog, &mut tree, &node, "field");
            assert!(
                event(
                    &mut dialog,
                    &mut tree,
                    &node,
                    Event::InputMethod(iced::advanced::input_method::Event::Preedit(
                        "漢".into(),
                        Some(0..3)
                    ))
                )
                .is_empty()
            );
            assert!(
                event(
                    &mut dialog,
                    &mut tree,
                    &node,
                    press(keyboard::key::Named::Enter, false)
                )
                .is_empty(),
                "Return belongs to IME composition"
            );
            let committed = event(
                &mut dialog,
                &mut tree,
                &node,
                Event::InputMethod(iced::advanced::input_method::Event::Commit("漢".into())),
            );
            assert!(
                matches!(committed.as_slice(), [text] if text.starts_with("edited:") && text.contains('漢'))
            );
            assert_eq!(
                event(
                    &mut dialog,
                    &mut tree,
                    &node,
                    press(keyboard::key::Named::Enter, false)
                ),
                ["save"]
            );
        }
    }
}
struct DefaultAction<'a, M, T, R> {
    content: Element<'a, M, T, R>,
    action: Option<M>,
    os: Os,
}
#[derive(Default)]
struct Context {
    editor: EditorFocus,
    composing: bool,
}
impl Operation for Context {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        operate(self);
    }
    fn text_input(
        &mut self,
        id: Option<&Id>,
        bounds: Rectangle,
        state: &mut dyn operation::TextInput,
    ) {
        self.editor.text_input(id, bounds, state);
    }
    fn focusable(
        &mut self,
        id: Option<&Id>,
        bounds: Rectangle,
        state: &mut dyn operation::Focusable,
    ) {
        self.editor.focusable(id, bounds, state);
    }
    fn custom(&mut self, id: Option<&Id>, bounds: Rectangle, state: &mut dyn std::any::Any) {
        self.editor.custom(id, bounds, state);
        if let Some(ime) = state.downcast_ref::<crate::gui::text_input::ImeMarker>() {
            self.composing |= ime.composing;
        }
    }
}
impl<M: Clone, T, R: renderer::Renderer> Widget<M, T, R> for DefaultAction<'_, M, T, R> {
    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }
    fn size_hint(&self) -> Size<Length> {
        self.content.as_widget().size_hint()
    }
    fn tag(&self) -> tree::Tag {
        self.content.as_widget().tag()
    }
    fn state(&self) -> tree::State {
        self.content.as_widget().state()
    }
    fn children(&self) -> Vec<Tree> {
        self.content.as_widget().children()
    }
    fn diff(&self, tree: &mut Tree) {
        self.content.as_widget().diff(tree);
    }
    fn layout(&mut self, tree: &mut Tree, renderer: &R, limits: &layout::Limits) -> layout::Node {
        self.content.as_widget_mut().layout(tree, renderer, limits)
    }
    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut R,
        theme: &T,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.content
            .as_widget()
            .draw(tree, renderer, theme, style, layout, cursor, viewport);
    }
    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &R,
        operation: &mut dyn Operation,
    ) {
        self.content
            .as_widget_mut()
            .operate(tree, layout, renderer, operation);
    }
    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &R,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, M>,
        viewport: &Rectangle,
    ) {
        let enter = match event {
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(keyboard::key::Named::Enter),
                modifiers,
                repeat,
                ..
            }) if modifiers.is_empty() && self.action.is_some() => Some(*repeat),
            _ => None,
        };
        let mut context = Context::default();
        if enter.is_some() {
            self.content
                .as_widget_mut()
                .operate(tree, layout, renderer, &mut context);
        }
        if enter == Some(true) && !context.editor.popup_open && !context.composing {
            shell.capture_event();
            return;
        }
        if self.os == Os::Mac
            && !context.editor.focused
            && !context.editor.popup_open
            && !context.composing
            && let Some(repeat) = enter
            && !shell.is_event_captured()
        {
            if !repeat {
                shell.publish(self.action.as_ref().unwrap().clone());
            }
            shell.capture_event();
            shell.request_redraw();
            return;
        }
        self.content.as_widget_mut().update(
            tree, event, layout, cursor, renderer, clipboard, shell, viewport,
        );
        if !context.editor.popup_open
            && !context.composing
            && !shell.is_event_captured()
            && let Some(repeat) = enter
        {
            if !repeat {
                shell.publish(self.action.as_ref().unwrap().clone());
            }
            shell.capture_event();
            shell.request_redraw();
        }
    }
    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &R,
    ) -> mouse::Interaction {
        self.content
            .as_widget()
            .mouse_interaction(tree, layout, cursor, viewport, renderer)
    }
    fn overlay<'a>(
        &'a mut self,
        tree: &'a mut Tree,
        layout: Layout<'a>,
        renderer: &R,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'a, M, T, R>> {
        self.content
            .as_widget_mut()
            .overlay(tree, layout, renderer, viewport, translation)
    }
}
