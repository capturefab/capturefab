//! App shortcuts can contain text in a native key event. Keep that text out of
//! editors before the application's keyboard subscription dispatches them.
use super::{
    Message,
    keys::{self, Action, Os},
};
use iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, overlay, renderer,
    widget::{Operation, Tree, tree},
};
use iced::{Element, Event, Length, Rectangle, Size, Vector, keyboard, mouse};

pub(super) fn guard(content: Element<'_, Message>) -> Element<'_, Message> {
    Element::new(Guard {
        content,
        os: Os::CURRENT,
    })
}

struct Guard<'a, Message, Theme, Renderer> {
    content: Element<'a, Message, Theme, Renderer>,
    os: Os,
}

impl<M, T, R: renderer::Renderer> Widget<M, T, R> for Guard<'_, M, T, R> {
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
        if let Event::Keyboard(keyboard::Event::KeyPressed {
            key,
            physical_key,
            modifiers,
            ..
        }) = event
            && let Some(chord) = keys::chord(key, *physical_key, *modifiers, self.os)
            && Action::find(&chord, false, self.os).is_some()
        {
            // The subscription receives captured events too. Do not publish a
            // second message, or one key would perform its action twice.
            shell.capture_event();
            return;
        }
        self.content.as_widget_mut().update(
            tree, event, layout, cursor, renderer, clipboard, shell, viewport,
        );
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

#[cfg(test)]
mod tests {
    use super::*;
    use iced::{
        Theme, event,
        keyboard::{
            Key, Location, Modifiers,
            key::{Code, Physical},
        },
    };

    fn press(character: &'static str, code: Code, modifiers: Modifiers) -> Event {
        Event::Keyboard(keyboard::Event::KeyPressed {
            key: Key::Character(character.into()),
            modified_key: Key::Character(character.into()),
            physical_key: Physical::Code(code),
            location: Location::Standard,
            modifiers,
            text: Some(character.into()),
            repeat: false,
        })
    }

    #[test]
    fn app_chords_do_not_insert_text_into_a_focused_editor() {
        for os in [Os::Mac, Os::Windows, Os::Nix] {
            let command = if os == Os::Mac {
                Modifiers::LOGO
            } else {
                Modifiers::CTRL
            };
            let content =
                iced::widget::TextInput::<String, Theme, ()>::new("", "128").on_input(|text| text);
            let mut guard = Guard {
                content: content.into(),
                os,
            };
            let mut tree = Tree::new(&guard as &dyn Widget<String, Theme, ()>);
            tree.state
                .downcast_mut::<iced::widget::text_input::State<()>>()
                .focus();
            let node = guard.layout(
                &mut tree,
                &(),
                &layout::Limits::new(Size::ZERO, Size::new(200.0, 30.0)),
            );
            let viewport = Rectangle::with_size(node.size());
            let mut clipboard = iced::advanced::clipboard::Null;
            let mut messages = Vec::new();
            let mut chords = vec![
                press("z", Code::KeyZ, command),
                press("z", Code::KeyZ, command | Modifiers::SHIFT),
                press("2", Code::Digit2, command),
                press("s", Code::KeyS, command),
            ];
            if os != Os::Mac {
                chords.push(press("y", Code::KeyY, command));
            }
            for event in chords {
                let mut shell = Shell::new(&mut messages);
                guard.update(
                    &mut tree,
                    &event,
                    Layout::new(&node),
                    mouse::Cursor::Unavailable,
                    &(),
                    &mut clipboard,
                    &mut shell,
                    &viewport,
                );
                assert_eq!(shell.event_status(), event::Status::Captured);
                assert!(
                    shell.is_empty(),
                    "the subscription must dispatch exactly once"
                );
            }
            // Ordinary typing still reaches the real text input and produces its
            // value, proving that the guard preserves the editor's focus/state.
            let mut shell = Shell::new(&mut messages);
            guard.update(
                &mut tree,
                &press("a", Code::KeyA, Modifiers::empty()),
                Layout::new(&node),
                mouse::Cursor::Unavailable,
                &(),
                &mut clipboard,
                &mut shell,
                &viewport,
            );
            assert_eq!(messages, ["128a"]);
        }
    }

    #[derive(Default)]
    struct TestClipboard {
        contents: Option<String>,
    }

    impl Clipboard for TestClipboard {
        fn read(&self, _: iced::advanced::clipboard::Kind) -> Option<String> {
            self.contents.clone()
        }

        fn write(&mut self, _: iced::advanced::clipboard::Kind, contents: String) {
            self.contents = Some(contents);
        }
    }

    #[test]
    fn native_selection_copy_cut_and_paste_reach_the_focused_editor() {
        // The native widget uses the build target's command modifier internally.
        for os in [Os::CURRENT] {
            let command = if os == Os::Mac {
                Modifiers::LOGO
            } else {
                Modifiers::CTRL
            };
            for (character, code) in [("c", Code::KeyC), ("x", Code::KeyX), ("v", Code::KeyV)] {
                let content = iced::widget::TextInput::<String, Theme, ()>::new("", "128")
                    .on_input(|text| text);
                let mut guard = Guard {
                    content: content.into(),
                    os,
                };
                let mut tree = Tree::new(&guard as &dyn Widget<String, Theme, ()>);
                tree.state
                    .downcast_mut::<iced::widget::text_input::State<()>>()
                    .focus();
                let node = guard.layout(
                    &mut tree,
                    &(),
                    &layout::Limits::new(Size::ZERO, Size::new(200.0, 30.0)),
                );
                let viewport = Rectangle::with_size(node.size());
                let mut clipboard = TestClipboard {
                    contents: Some("pasted".into()),
                };
                let mut messages = Vec::new();
                for event in [
                    Event::Keyboard(keyboard::Event::ModifiersChanged(command)),
                    press("a", Code::KeyA, command),
                    press(character, code, command),
                ] {
                    guard.update(
                        &mut tree,
                        &event,
                        Layout::new(&node),
                        mouse::Cursor::Unavailable,
                        &(),
                        &mut clipboard,
                        &mut Shell::new(&mut messages),
                        &viewport,
                    );
                }
                match character {
                    "c" => {
                        assert_eq!(clipboard.contents.as_deref(), Some("128"));
                        assert!(messages.is_empty());
                    }
                    "x" => {
                        assert_eq!(clipboard.contents.as_deref(), Some("128"));
                        assert_eq!(messages, [""]);
                    }
                    "v" => assert_eq!(messages, ["pasted"]),
                    _ => unreachable!(),
                }
            }
        }
    }
}
