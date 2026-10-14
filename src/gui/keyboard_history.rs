//! App shortcuts can contain text in a native key event. Keep that text out of
//! editors before the application's keyboard subscription dispatches them.
use super::{
    Message,
    keys::{self, Action, Os},
};
use iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, overlay, renderer,
    widget::{Id, Operation, Tree, operation, tree},
};
use iced::{Element, Event, Length, Rectangle, Size, Vector, keyboard, mouse, window};

mod focus_return;
pub(super) use focus_return::FocusReturn;
mod default_action;
pub(super) use default_action::default_action;
mod content_focus;
pub(super) use content_focus::{Area, scope as content_scope};
#[cfg(test)]
mod alert_tests;
#[cfg(test)]
mod focus_tests;
#[cfg(test)]
mod report_tests;
#[cfg(test)]
mod window_focus_tests;

/// Native Edit menu commands go through the same editor and clipboard as keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum EditCommand {
    Cut,
    Copy,
    Paste,
    SelectAll,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct EditRequest {
    pub serial: u64,
    pub command: EditCommand,
}

pub(super) fn guard(
    content: Element<'_, Message>,
    edit: Option<EditRequest>,
    desktop_active: bool,
) -> Element<'_, Message> {
    make_guard(content, edit, desktop_active, false, None)
}

/// Only the workbench document reports selection emphasis. Native Settings and
/// modal/escape guards retain their ordinary editor and popup reporting.
pub(super) fn main_guard(
    content: Element<'_, Message>,
    edit: Option<EditRequest>,
    desktop_active: bool,
    suspend_content_focus: bool,
) -> Element<'_, Message> {
    make_guard(
        content,
        edit,
        desktop_active,
        suspend_content_focus,
        Some(Message::ContentFocused),
    )
}

fn make_guard(
    content: Element<'_, Message>,
    edit: Option<EditRequest>,
    desktop_active: bool,
    suspend_content_focus: bool,
    on_content_focus: Option<fn(Area) -> Message>,
) -> Element<'_, Message> {
    Element::new(Guard {
        content,
        os: Os::CURRENT,
        background: false,
        desktop_active,
        edit,
        on_editor_focus: Some(Message::EditorFocused),
        on_popup: Some(Message::PopupOpen),
        on_escape: Some(escape_message),
        on_content_focus,
        suspend_content_focus,
    })
}

/// An alert outside the normal document guard must forward Escape, without
/// replacing the editor and popup metadata retained by its obscured parent.
pub(super) fn escape_guard(content: Element<'_, Message>) -> Element<'_, Message> {
    Element::new(Guard {
        content,
        os: Os::CURRENT,
        background: false,
        desktop_active: false,
        edit: None,
        on_editor_focus: None,
        on_popup: None,
        on_escape: Some(escape_message),
        on_content_focus: None,
        suspend_content_focus: false,
    })
}

fn escape_message(captured: bool) -> Message {
    Message::Key {
        chord: keys::Chord {
            key: keys::Key::Escape,
            mods: keys::Mods::NONE,
        },
        captured,
    }
}

/// A modal's parent keeps its state and focus for when the sheet closes, but
/// cannot receive input, steal the IME, or place a popup above the sheet.
pub(super) fn background(content: Element<'_, Message>, inactive: bool) -> Element<'_, Message> {
    Element::new(Guard {
        content,
        os: Os::CURRENT,
        background: inactive,
        desktop_active: false,
        edit: None,
        on_editor_focus: None,
        on_popup: None,
        on_escape: None,
        on_content_focus: None,
        suspend_content_focus: false,
    })
}

struct Guard<'a, Message, Theme, Renderer> {
    content: Element<'a, Message, Theme, Renderer>,
    os: Os,
    background: bool,
    desktop_active: bool,
    edit: Option<EditRequest>,
    on_editor_focus: Option<fn(bool) -> Message>,
    on_popup: Option<fn(bool) -> Message>,
    on_escape: Option<fn(bool) -> Message>,
    on_content_focus: Option<fn(Area) -> Message>,
    suspend_content_focus: bool,
}

#[derive(Default)]
struct State {
    last_edit: Option<u64>,
    modifiers: keyboard::Modifiers,
    editor_focused: Option<bool>,
    popup_open: Option<bool>,
    content_focus: content_focus::Tracker,
}

/// Text inputs expose their editor immediately before their focus state.
/// Buttons expose only focus, so they do not enable native editing commands.
#[derive(Default)]
struct EditorFocus {
    is_editor: bool,
    focused: bool,
    popup_open: bool,
}

impl Operation for EditorFocus {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        operate(self);
    }

    fn text_input(&mut self, _: Option<&Id>, _: Rectangle, _: &mut dyn operation::TextInput) {
        self.is_editor = true;
    }

    fn focusable(&mut self, _: Option<&Id>, _: Rectangle, state: &mut dyn operation::Focusable) {
        self.focused |= self.is_editor && state.is_focused();
        self.is_editor = false;
    }

    fn custom(&mut self, _: Option<&Id>, _: Rectangle, state: &mut dyn std::any::Any) {
        if let Some(popup) = state.downcast_ref::<super::pick_list::PopupMarker>() {
            self.popup_open |= popup.open;
        }
    }
}

struct ClosePopups;

impl Operation for ClosePopups {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        operate(self);
    }

    fn custom(&mut self, _: Option<&Id>, _: Rectangle, state: &mut dyn std::any::Any) {
        if let Some(popup) = state.downcast_mut::<super::pick_list::PopupMarker>() {
            popup.open = false;
        }
    }
}

impl<M, T, R: renderer::Renderer> Widget<M, T, R> for Guard<'_, M, T, R> {
    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }
    fn size_hint(&self) -> Size<Length> {
        self.content.as_widget().size_hint()
    }
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }
    fn state(&self) -> tree::State {
        tree::State::new(State::default())
    }
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(self.content.as_widget())]
    }
    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(&[self.content.as_widget()]);
    }

    fn layout(&mut self, tree: &mut Tree, renderer: &R, limits: &layout::Limits) -> layout::Node {
        self.content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
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
        self.content.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            if self.background {
                mouse::Cursor::Unavailable
            } else {
                cursor
            },
            viewport,
        );
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &R,
        operation: &mut dyn Operation,
    ) {
        if !self.background {
            self.content.as_widget_mut().operate(
                &mut tree.children[0],
                layout,
                renderer,
                operation,
            );
        }
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
        let state = tree.state.downcast_mut::<State>();
        match event {
            Event::Keyboard(keyboard::Event::ModifiersChanged(modifiers))
            | Event::Keyboard(keyboard::Event::KeyPressed { modifiers, .. })
            | Event::Keyboard(keyboard::Event::KeyReleased { modifiers, .. }) => {
                state.modifiers = *modifiers;
            }
            _ => {}
        }
        if matches!(event, Event::Window(window::Event::Focused)) {
            // Each native window may preserve its editor focus while another
            // window is frontmost. Re-publish availability when it returns.
            state.editor_focused = None;
            state.popup_open = None;
            if !self.suspend_content_focus {
                state.content_focus.reported = None;
            }
        }
        if self.background {
            if matches!(event, Event::Window(window::Event::RedrawRequested(_))) {
                // Dismiss a menu belonging to the obscured document without
                // removing the field/control focus restored after the sheet.
                self.content.as_widget_mut().operate(
                    &mut tree.children[0],
                    layout,
                    renderer,
                    &mut ClosePopups,
                );
            }
            // Bookkeeping cannot edit content: keep modifiers current and
            // clear an IME the OS closed when the sheet took over its input.
            if !matches!(
                event,
                Event::Window(_)
                    | Event::Keyboard(keyboard::Event::ModifiersChanged(_))
                    | Event::InputMethod(iced::advanced::input_method::Event::Closed)
            ) {
                return;
            }
            let ime = shell.input_method().clone();
            self.content.as_widget_mut().update(
                &mut tree.children[0],
                event,
                layout,
                mouse::Cursor::Unavailable,
                renderer,
                clipboard,
                shell,
                viewport,
            );
            *shell.input_method_mut() = ime;
            return;
        }
        if matches!(
            event,
            Event::Keyboard(keyboard::Event::KeyPressed { .. })
                | Event::Mouse(mouse::Event::ButtonPressed(_))
                | Event::Touch(iced::touch::Event::FingerPressed { .. })
        ) {
            focus_return::cancel(&mut self.content, &mut tree.children[0], layout, renderer);
        }
        if let Event::Keyboard(keyboard::Event::KeyPressed {
            key,
            physical_key,
            modifiers,
            ..
        }) = event
            && let Some(chord) = keys::chord(key, *physical_key, *modifiers, self.os)
            && !self.desktop_active
            && Action::find(&chord, false, self.os).is_some()
        {
            // The subscription receives captured events too. Do not publish a
            // second message, or one key would perform its action twice.
            shell.capture_event();
            return;
        }
        // An overlay-captured Escape never reaches this widget. If a popup was
        // opened earlier in this same batch, its live marker still protects the
        // enclosing sheet even before iced has constructed the new overlay.
        // Read before forwarding: the child may close its popup during update.
        let escape = !self.desktop_active
            && self.on_escape.is_some()
            && matches!(
                event,
                Event::Keyboard(keyboard::Event::KeyPressed {
                    key: keyboard::Key::Named(keyboard::key::Named::Escape),
                    modifiers,
                    ..
                }) if modifiers.is_empty()
            );
        let popup_was_open = if escape {
            let mut focus = EditorFocus::default();
            self.content.as_widget_mut().operate(
                &mut tree.children[0],
                layout,
                renderer,
                &mut focus,
            );
            focus.popup_open
        } else {
            false
        };
        if matches!(event, Event::Window(window::Event::RedrawRequested(_))) {
            if focus_return::recover(&mut self.content, &mut tree.children[0], layout, renderer) {
                shell.request_redraw();
            }
            if let Some(request) = self.edit
                && state.last_edit != Some(request.serial)
            {
                state.last_edit = Some(request.serial);
                let (character, code) = match request.command {
                    EditCommand::Cut => ("x", keyboard::key::Code::KeyX),
                    EditCommand::Copy => ("c", keyboard::key::Code::KeyC),
                    EditCommand::Paste => ("v", keyboard::key::Code::KeyV),
                    EditCommand::SelectAll => ("a", keyboard::key::Code::KeyA),
                };
                let command = if self.os == Os::Mac {
                    keyboard::Modifiers::LOGO
                } else {
                    keyboard::Modifiers::CTRL
                };
                let key = keyboard::Key::Character(character.into());
                for synthetic in [
                    keyboard::Event::ModifiersChanged(command),
                    keyboard::Event::KeyPressed {
                        key: key.clone(),
                        modified_key: key.clone(),
                        physical_key: keyboard::key::Physical::Code(code),
                        location: keyboard::Location::Standard,
                        modifiers: command,
                        text: None,
                        repeat: false,
                    },
                    keyboard::Event::KeyReleased {
                        key: key.clone(),
                        modified_key: key,
                        physical_key: keyboard::key::Physical::Code(code),
                        location: keyboard::Location::Standard,
                        modifiers: command,
                    },
                    keyboard::Event::ModifiersChanged(state.modifiers),
                ] {
                    // Each event gets its own capture status. A captured edit
                    // must neither stop the following modifier reset nor hide
                    // the actual redraw from the rest of the widget tree.
                    let mut messages = Vec::new();
                    let mut editing = Shell::new(&mut messages);
                    self.content.as_widget_mut().update(
                        &mut tree.children[0],
                        &Event::Keyboard(synthetic),
                        layout,
                        cursor,
                        renderer,
                        clipboard,
                        &mut editing,
                        viewport,
                    );
                    shell.request_redraw_at(editing.redraw_request());
                    if editing.is_layout_invalid() {
                        shell.invalidate_layout();
                    }
                    if editing.are_widgets_invalid() {
                        shell.invalidate_widgets();
                    }
                    for message in messages {
                        shell.publish(message);
                    }
                }
            }
            if self.on_editor_focus.is_some() || self.on_popup.is_some() {
                let mut focus = EditorFocus::default();
                self.content.as_widget_mut().operate(
                    &mut tree.children[0],
                    layout,
                    renderer,
                    &mut focus,
                );
                if state.editor_focused != Some(focus.focused)
                    && let Some(on_focus) = self.on_editor_focus
                {
                    state.editor_focused = Some(focus.focused);
                    shell.publish(on_focus(focus.focused));
                }
                if state.popup_open != Some(focus.popup_open)
                    && let Some(on_popup) = self.on_popup
                {
                    state.popup_open = Some(focus.popup_open);
                    shell.publish(on_popup(focus.popup_open));
                }
            }
            if !self.suspend_content_focus
                && let Some(on_focus) = self.on_content_focus
            {
                let area = content_focus::focused_area(
                    &mut self.content,
                    &mut tree.children[0],
                    layout,
                    renderer,
                )
                .unwrap_or(state.content_focus.pointer_hint);
                if state.content_focus.reported != Some(area) {
                    state.content_focus.reported = Some(area);
                    shell.publish(on_focus(area));
                }
            }
        }
        let pointer = !self.suspend_content_focus
            && self.on_content_focus.is_some()
            && content_focus::pointer_position(event, cursor).is_some();
        if pointer {
            content_focus::clear_pointer(
                &mut self.content,
                &mut tree.children[0],
                layout,
                renderer,
            );
        }
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
        if pointer {
            let area = content_focus::pointer_area(
                &mut self.content,
                &mut tree.children[0],
                layout,
                renderer,
            )
            .unwrap_or(Area::Other);
            if state.content_focus.pointer_hint != area {
                state.content_focus.pointer_hint = area;
                // A blank document press may be ignored by every child, so
                // explicitly schedule the redraw that reports its new area.
                shell.request_redraw();
            }
        }
        if escape
            && !popup_was_open
            && let Some(on_escape) = self.on_escape
        {
            shell.publish(on_escape(
                shell.event_status() == iced::event::Status::Captured,
            ));
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
        self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            if self.background {
                mouse::Cursor::Unavailable
            } else {
                cursor
            },
            viewport,
            renderer,
        )
    }

    fn overlay<'a>(
        &'a mut self,
        tree: &'a mut Tree,
        layout: Layout<'a>,
        renderer: &R,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'a, M, T, R>> {
        if self.background {
            None
        } else {
            self.content.as_widget_mut().overlay(
                &mut tree.children[0],
                layout,
                renderer,
                viewport,
                translation,
            )
        }
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

    pub(super) fn named_press(key: keyboard::key::Named, modifiers: Modifiers) -> Event {
        let key = Key::Named(key);
        Event::Keyboard(keyboard::Event::KeyPressed {
            key: key.clone(),
            modified_key: key,
            physical_key: Physical::Unidentified(keyboard::key::NativeCode::Unidentified),
            location: Location::Standard,
            modifiers,
            text: None,
            repeat: false,
        })
    }

    #[test]
    fn plain_escape_reports_child_capture_once_and_desktop_menus_own_it() {
        let content =
            iced::widget::TextInput::<String, Theme, ()>::new("", "128").on_input(|text| text);
        let mut guard = Guard {
            content: content.into(),
            os: Os::CURRENT,
            background: false,
            desktop_active: false,
            edit: None,
            on_editor_focus: None,
            on_popup: None,
            on_escape: Some(|captured| format!("escape:{captured}")),
            on_content_focus: None,
            suspend_content_focus: false,
        };
        let mut tree = Tree::new(&guard as &dyn Widget<String, Theme, ()>);
        tree.children[0]
            .state
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
        guard.update(
            &mut tree,
            &named_press(keyboard::key::Named::Escape, Modifiers::empty()),
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &(),
            &mut clipboard,
            &mut Shell::new(&mut messages),
            &viewport,
        );
        assert_eq!(
            messages,
            ["escape:true"],
            "a field's captured Escape still closes a sheet"
        );
        messages.clear();
        guard.desktop_active = true;
        guard.update(
            &mut tree,
            &named_press(keyboard::key::Named::Escape, Modifiers::empty()),
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &(),
            &mut clipboard,
            &mut Shell::new(&mut messages),
            &viewport,
        );
        assert!(
            messages.is_empty(),
            "desktop menu Escape cannot also close the sheet"
        );
        guard.desktop_active = false;
        guard.update(
            &mut tree,
            &named_press(keyboard::key::Named::Escape, Modifiers::SHIFT),
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &(),
            &mut clipboard,
            &mut Shell::new(&mut messages),
            &viewport,
        );
        assert!(
            messages.is_empty(),
            "modified Escape is not the sheet cancellation command"
        );
    }

    #[test]
    fn popup_escape_in_the_opening_batch_preserves_the_enclosing_sheet() {
        let content = super::super::pick_list::PickList::<_, _, ()>::new(
            ["One", "Two"],
            Some("One"),
            |value| value.to_string(),
        )
        .id("choice");
        let mut guard = Guard {
            content: content.into(),
            os: Os::CURRENT,
            background: false,
            desktop_active: false,
            edit: None,
            on_editor_focus: None,
            on_popup: None,
            on_escape: Some(|captured| format!("escape:{captured}")),
            on_content_focus: None,
            suspend_content_focus: false,
        };
        let mut tree = Tree::new(&guard as &dyn Widget<String, Theme, ()>);
        let node = guard.layout(
            &mut tree,
            &(),
            &layout::Limits::new(Size::ZERO, Size::new(200.0, 30.0)),
        );
        let viewport = Rectangle::with_size(Size::new(300.0, 300.0));
        guard.operate(
            &mut tree,
            Layout::new(&node),
            &(),
            &mut operation::focusable::focus::<()>(Id::new("choice")),
        );
        let mut clipboard = iced::advanced::clipboard::Null;
        let mut messages = Vec::new();
        for key in [keyboard::key::Named::Space, keyboard::key::Named::Escape] {
            let mut shell = Shell::new(&mut messages);
            guard.update(
                &mut tree,
                &named_press(key, Modifiers::empty()),
                Layout::new(&node),
                mouse::Cursor::Unavailable,
                &(),
                &mut clipboard,
                &mut shell,
                &viewport,
            );
            assert_eq!(shell.event_status(), event::Status::Captured);
        }
        assert!(
            messages.is_empty(),
            "dropdown dismissal cannot publish enclosing Escape"
        );
        assert!(
            guard
                .overlay(&mut tree, Layout::new(&node), &(), &viewport, Vector::ZERO)
                .is_none()
        );
        guard.update(
            &mut tree,
            &named_press(keyboard::key::Named::Escape, Modifiers::empty()),
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &(),
            &mut clipboard,
            &mut Shell::new(&mut messages),
            &viewport,
        );
        assert_eq!(
            messages,
            ["escape:true"],
            "the next Escape reaches sheet cancellation"
        );

        // Opening a sheet over a dropdown must not resurrect that dropdown
        // after the sheet closes; the original control remains focused.
        messages.clear();
        guard.operate(
            &mut tree,
            Layout::new(&node),
            &(),
            &mut operation::focusable::focus::<()>(Id::new("choice")),
        );
        guard.update(
            &mut tree,
            &named_press(keyboard::key::Named::Space, Modifiers::empty()),
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &(),
            &mut clipboard,
            &mut Shell::new(&mut messages),
            &viewport,
        );
        guard.background = true;
        guard.update(
            &mut tree,
            &redraw(),
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &(),
            &mut clipboard,
            &mut Shell::new(&mut messages),
            &viewport,
        );
        guard.background = false;
        assert!(
            guard
                .overlay(&mut tree, Layout::new(&node), &(), &viewport, Vector::ZERO)
                .is_none()
        );
        guard.update(
            &mut tree,
            &named_press(keyboard::key::Named::Space, Modifiers::empty()),
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &(),
            &mut clipboard,
            &mut Shell::new(&mut messages),
            &viewport,
        );
        let mut focus = EditorFocus::default();
        guard.operate(&mut tree, Layout::new(&node), &(), &mut focus);
        assert!(
            focus.popup_open,
            "the parent's focused control is restored after the sheet"
        );
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
                background: false,
                desktop_active: false,
                edit: None,
                on_editor_focus: None,
                on_popup: None,
                on_escape: None,
                on_content_focus: None,
                suspend_content_focus: false,
            };
            let mut tree = Tree::new(&guard as &dyn Widget<String, Theme, ()>);
            tree.children[0]
                .state
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
                    background: false,
                    desktop_active: false,
                    edit: None,
                    on_editor_focus: None,
                    on_popup: None,
                    on_escape: None,
                    on_content_focus: None,
                    suspend_content_focus: false,
                };
                let mut tree = Tree::new(&guard as &dyn Widget<String, Theme, ()>);
                tree.children[0]
                    .state
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
    fn redraw() -> Event {
        Event::Window(window::Event::RedrawRequested(std::time::Instant::now()))
    }

    #[test]
    fn modal_parent_cannot_receive_typing_ime_or_editing_and_retains_focus() {
        let content =
            iced::widget::TextInput::<String, Theme, ()>::new("", "128").on_input(|text| text);
        let mut guard = Guard {
            content: content.into(),
            os: Os::CURRENT,
            background: true,
            desktop_active: false,
            edit: None,
            on_editor_focus: None,
            on_popup: None,
            on_escape: None,
            on_content_focus: None,
            suspend_content_focus: false,
        };
        let mut tree = Tree::new(&guard as &dyn Widget<String, Theme, ()>);
        tree.children[0]
            .state
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
            press("a", Code::KeyA, Modifiers::empty()),
            Event::InputMethod(iced::advanced::input_method::Event::Commit(
                "composition".into(),
            )),
            press(
                "v",
                Code::KeyV,
                if Os::CURRENT == Os::Mac {
                    Modifiers::LOGO
                } else {
                    Modifiers::CTRL
                },
            ),
            Event::Keyboard(keyboard::Event::ModifiersChanged(Modifiers::empty())),
            redraw(),
        ] {
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
            assert!(
                !shell.input_method().is_enabled(),
                "the parent cannot activate its IME"
            );
        }
        assert!(messages.is_empty());
        let mut focus = EditorFocus::default();
        guard.operate(&mut tree, Layout::new(&node), &(), &mut focus);
        assert!(
            !focus.focused,
            "inactive editors are excluded from menu commands and tab order"
        );
        assert!(
            tree.children[0]
                .state
                .downcast_ref::<iced::widget::text_input::State<()>>()
                .is_focused(),
            "the parent's focus is retained for restoration"
        );
        guard.background = false;
        guard.update(
            &mut tree,
            &press("b", Code::KeyB, Modifiers::empty()),
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &(),
            &mut clipboard,
            &mut Shell::new(&mut messages),
            &viewport,
        );
        assert_eq!(messages, ["128b"]);
    }

    #[test]
    fn native_edit_requests_use_editor_selection_and_clipboard() {
        for command in [EditCommand::Cut, EditCommand::Copy, EditCommand::Paste] {
            let content =
                iced::widget::TextInput::<String, Theme, ()>::new("", "128").on_input(|text| text);
            let mut guard = Guard {
                content: content.into(),
                os: Os::CURRENT,
                background: false,
                desktop_active: false,
                edit: Some(EditRequest {
                    serial: 1,
                    command: EditCommand::SelectAll,
                }),
                on_editor_focus: None,
                on_popup: None,
                on_escape: None,
                on_content_focus: None,
                suspend_content_focus: false,
            };
            let mut tree = Tree::new(&guard as &dyn Widget<String, Theme, ()>);
            tree.children[0]
                .state
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
            guard.update(
                &mut tree,
                &redraw(),
                Layout::new(&node),
                mouse::Cursor::Unavailable,
                &(),
                &mut clipboard,
                &mut Shell::new(&mut messages),
                &viewport,
            );
            guard.edit = Some(EditRequest { serial: 2, command });
            guard.update(
                &mut tree,
                &redraw(),
                Layout::new(&node),
                mouse::Cursor::Unavailable,
                &(),
                &mut clipboard,
                &mut Shell::new(&mut messages),
                &viewport,
            );
            match command {
                EditCommand::Copy => {
                    assert_eq!(clipboard.contents.as_deref(), Some("128"));
                    assert!(messages.is_empty());
                }
                EditCommand::Cut => {
                    assert_eq!(clipboard.contents.as_deref(), Some("128"));
                    assert_eq!(messages, [""]);
                }
                EditCommand::Paste => assert_eq!(messages, ["pasted"]),
                EditCommand::SelectAll => unreachable!(),
            }
        }
    }

    #[test]
    fn menu_edit_runs_once_across_view_diff_and_restores_real_modifiers() {
        let content =
            iced::widget::TextInput::<String, Theme, ()>::new("", "128").on_input(|text| text);
        let mut guard = Guard {
            content: content.into(),
            os: Os::CURRENT,
            background: false,
            desktop_active: false,
            edit: Some(EditRequest {
                serial: 1,
                command: EditCommand::Paste,
            }),
            on_editor_focus: None,
            on_popup: None,
            on_escape: None,
            on_content_focus: None,
            suspend_content_focus: false,
        };
        let mut tree = Tree::new(&guard as &dyn Widget<String, Theme, ()>);
        tree.children[0]
            .state
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
        let mut shell = Shell::new(&mut messages);
        guard.update(
            &mut tree,
            &redraw(),
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &(),
            &mut clipboard,
            &mut shell,
            &viewport,
        );
        assert_eq!(
            shell.event_status(),
            event::Status::Ignored,
            "synthetic capture must not swallow redraw"
        );
        assert_eq!(messages, ["128pasted"]);
        guard.content = iced::widget::TextInput::<String, Theme, ()>::new("", "128pasted")
            .on_input(|text| text)
            .into();
        guard.diff(&mut tree);
        guard.update(
            &mut tree,
            &redraw(),
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &(),
            &mut clipboard,
            &mut Shell::new(&mut messages),
            &viewport,
        );
        assert_eq!(messages, ["128pasted"], "one request produces one edit");
        guard.update(
            &mut tree,
            &press("a", Code::KeyA, Modifiers::empty()),
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &(),
            &mut clipboard,
            &mut Shell::new(&mut messages),
            &viewport,
        );
        assert_eq!(
            messages,
            ["128pasted", "128pasteda"],
            "Command/Ctrl must be released after a menu edit"
        );
    }
    #[test]
    fn editor_menu_state_excludes_modal_parent_and_restores_after_close() {
        fn content(inactive: bool) -> Element<'static, String, Theme, ()> {
            Element::new(Guard {
                content: iced::widget::TextInput::new("", "128")
                    .on_input(|text| text)
                    .into(),
                os: Os::CURRENT,
                background: inactive,
                desktop_active: false,
                edit: None,
                on_editor_focus: None,
                on_popup: None,
                on_escape: None,
                on_content_focus: None,
                suspend_content_focus: false,
            })
        }
        let mut guard = Guard {
            content: content(true),
            os: Os::CURRENT,
            background: false,
            desktop_active: false,
            edit: Some(EditRequest {
                serial: 1,
                command: EditCommand::Paste,
            }),
            on_editor_focus: Some(|focused| format!("editor:{focused}")),
            on_popup: None,
            on_escape: None,
            on_content_focus: None,
            suspend_content_focus: false,
        };
        let mut tree = Tree::new(&guard as &dyn Widget<String, Theme, ()>);
        tree.children[0].children[0]
            .state
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
        guard.update(
            &mut tree,
            &redraw(),
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &(),
            &mut clipboard,
            &mut Shell::new(&mut messages),
            &viewport,
        );
        assert_eq!(
            messages,
            ["editor:false"],
            "only the current modal may receive a menu edit"
        );
        guard.content = content(false);
        guard.diff(&mut tree);
        for _ in 0..2 {
            guard.update(
                &mut tree,
                &redraw(),
                Layout::new(&node),
                mouse::Cursor::Unavailable,
                &(),
                &mut clipboard,
                &mut Shell::new(&mut messages),
                &viewport,
            );
        }
        assert_eq!(
            messages,
            ["editor:false", "editor:true"],
            "focus is restored and only changes are reported"
        );
    }
}
