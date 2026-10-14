//! Desktop buttons retain iced's pointer behavior and styling while supporting
//! the keyboard focus, activation and disabled traversal expected on every OS.
use super::{accessibility, motion, style};
use iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, overlay, renderer,
    widget::{Id, Operation, Tree, operation::Focusable, tree},
};
pub(super) use iced::widget::button::{Status, Style};
use iced::{
    Border, Color, Element, Event, Length, Padding, Rectangle, Size, Theme, Vector, keyboard,
    mouse, touch, window,
};

type KeyHandler<'a, M> = dyn Fn(&keyboard::Key, keyboard::Modifiers) -> Option<Vec<M>> + 'a;

pub(super) struct Button<'a, M, R = iced::Renderer>
where
    R: renderer::Renderer,
{
    inner: iced::widget::button::Button<'a, M, Theme, R>,
    on_press: Option<M>,
    id: Option<Id>,
    on_key_press: Option<Box<KeyHandler<'a, M>>>,
    name: Option<String>,
    role: accessibility::Role,
    checked: Option<bool>,
    selected: Option<bool>,
    expanded: Option<bool>,
    value: Option<String>,
    numeric: Option<(f64, f64, f64, f64)>,
    increment: Option<M>,
    decrement: Option<M>,
    set_value: Option<accessibility::ValueHandler<M>>,
}

pub(super) fn button<'a, M: Clone + 'static>(content: impl Into<Element<'a, M>>) -> Button<'a, M> {
    Button::new(content)
}

impl<'a, M: Clone + 'static, R: renderer::Renderer + 'a> Button<'a, M, R> {
    pub(super) fn new(content: impl Into<Element<'a, M, Theme, R>>) -> Self {
        Self {
            inner: iced::widget::button::Button::new(content),
            on_press: None,
            id: None,
            on_key_press: None,
            name: None,
            role: accessibility::Role::Button,
            checked: None,
            selected: None,
            expanded: None,
            value: None,
            numeric: None,
            increment: None,
            decrement: None,
            set_value: None,
        }
    }
    pub(super) fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }
    pub(super) fn role(mut self, role: accessibility::Role) -> Self {
        self.role = role;
        self
    }
    pub(super) fn checked(mut self, checked: bool) -> Self {
        self.checked = Some(checked);
        self
    }
    pub(super) fn selected(mut self, selected: bool) -> Self {
        self.selected = Some(selected);
        self
    }
    pub(super) fn expanded(mut self, expanded: bool) -> Self {
        self.expanded = Some(expanded);
        self
    }
    pub(super) fn value(mut self, value: impl Into<String>) -> Self {
        self.value = Some(value.into());
        self
    }
    pub(super) fn numeric_value(mut self, value: f64, min: f64, max: f64, step: f64) -> Self {
        self.numeric = Some((value, min, max, step));
        self
    }
    pub(super) fn increment(mut self, message: M) -> Self {
        self.increment = Some(message);
        self
    }
    pub(super) fn decrement(mut self, message: M) -> Self {
        self.decrement = Some(message);
        self
    }
    pub(super) fn set_value(
        mut self,
        handler: impl Fn(String) -> M + Send + Sync + 'static,
    ) -> Self {
        self.set_value = Some(std::sync::Arc::new(handler));
        self
    }
    pub(super) fn id(mut self, id: impl Into<Id>) -> Self {
        self.id = Some(id.into());
        self
    }
    pub(super) fn width(mut self, width: impl Into<Length>) -> Self {
        self.inner = self.inner.width(width);
        self
    }
    pub(super) fn height(mut self, height: impl Into<Length>) -> Self {
        self.inner = self.inner.height(height);
        self
    }
    pub(super) fn padding(mut self, padding: impl Into<Padding>) -> Self {
        self.inner = self.inner.padding(padding);
        self
    }
    pub(super) fn style(mut self, style: impl Fn(&Theme, Status) -> Style + 'a) -> Self {
        self.inner = self.inner.style(style);
        self
    }
    pub(super) fn on_press(self, message: M) -> Self {
        self.on_press_maybe(Some(message))
    }
    pub(super) fn on_press_maybe(mut self, message: Option<M>) -> Self {
        self.on_press = message.clone();
        self.inner = self.inner.on_press_maybe(message);
        self
    }
    /// Extra keys for a compound control, such as a slider. Returning messages
    /// captures the event before the workbench sees it, even at a value bound.
    pub(super) fn on_key_press(
        mut self,
        handler: impl Fn(&keyboard::Key, keyboard::Modifiers) -> Option<Vec<M>> + 'a,
    ) -> Self {
        self.on_key_press = Some(Box::new(handler));
        self
    }
}

struct State<M> {
    focused: bool,
    space_held: bool,
    window_focused: bool,
    control: accessibility::Control<M>,
    focus_return: super::keyboard_history::FocusReturn,
}

impl<M> Default for State<M> {
    fn default() -> Self {
        let control = accessibility::Control::new(accessibility::Role::Button);
        let focus_return = super::keyboard_history::FocusReturn::new(control.focus_id.clone());
        Self {
            focused: false,
            space_held: false,
            window_focused: true,
            control,
            focus_return,
        }
    }
}

impl<M> Focusable for State<M> {
    fn is_focused(&self) -> bool {
        self.focused
    }
    fn focus(&mut self) {
        self.focused = true;
    }
    fn unfocus(&mut self) {
        self.focused = false;
        self.space_held = false;
    }
}

impl<'a, M: Clone + 'static, R: renderer::Renderer + 'a> Widget<M, Theme, R> for Button<'a, M, R> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State<M>>()
    }
    fn state(&self) -> tree::State {
        tree::State::new(State::<M>::default())
    }
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.inner as &dyn Widget<M, Theme, R>)]
    }
    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(&[&self.inner as &dyn Widget<M, Theme, R>]);
        if self.on_press.is_none() {
            let state = tree.state.downcast_mut::<State<M>>();
            state.focus_return.pending |= state.focused;
            state.unfocus();
        }
    }
    fn size(&self) -> Size<Length> {
        self.inner.size()
    }
    fn size_hint(&self) -> Size<Length> {
        self.inner.size_hint()
    }
    fn layout(&mut self, tree: &mut Tree, renderer: &R, limits: &layout::Limits) -> layout::Node {
        self.inner.layout(&mut tree.children[0], renderer, limits)
    }
    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &R,
        operation: &mut dyn Operation,
    ) {
        let state = tree.state.downcast_mut::<State<M>>();
        let mut node = accessibility::Node::new(self.role);
        node.set_label(self.name.as_deref().unwrap_or("Button"));
        if let Some(checked) = self.checked {
            node.set_toggled(if checked {
                accessibility::Toggled::True
            } else {
                accessibility::Toggled::False
            });
        }
        if let Some(selected) = self.selected {
            node.set_selected(selected);
        }
        if let Some(expanded) = self.expanded {
            node.set_expanded(expanded);
        }
        if let Some(value) = &self.value {
            node.set_value(value.clone());
        }
        if let Some((value, min, max, step)) = self.numeric {
            node.set_numeric_value(value);
            node.set_min_numeric_value(min);
            node.set_max_numeric_value(max);
            node.set_numeric_value_step(step);
        }
        let enabled = self.on_press.is_some();
        if !enabled {
            node.set_disabled();
        }
        let control = &mut state.control;
        if let Some(id) = &self.id {
            control.focus_id = id.clone();
        }
        control.node = node;
        control.focused = state.focused;
        control.activate = self.on_press.clone();
        control.increment = enabled.then(|| self.increment.clone()).flatten();
        control.decrement = enabled.then(|| self.decrement.clone()).flatten();
        control.set_value = enabled.then(|| self.set_value.clone()).flatten();
        let focus_id = control.focus_id.clone();
        state.focus_return.id = focus_id.clone();
        state.focus_return.available = enabled;
        operation.custom(Some(&focus_id), layout.bounds(), &mut state.focus_return);
        operation.custom(Some(&focus_id), layout.bounds(), control);
        if enabled {
            operation.focusable(Some(&focus_id), layout.bounds(), state);
        }
        self.inner
            .operate(&mut tree.children[0], layout, renderer, operation);
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
        let state = tree.state.downcast_mut::<State<M>>();
        match event {
            // Pointer interaction leaves keyboard focus. Tab returns to the
            // first logical control, and a clicked field owns subsequent keys.
            Event::Mouse(mouse::Event::ButtonPressed(_))
            | Event::Touch(touch::Event::FingerPressed { .. }) => {
                if state.focused {
                    state.unfocus();
                    shell.request_redraw();
                }
            }
            Event::Window(window::Event::Unfocused) => {
                state.window_focused = false;
                state.space_held = false;
                shell.request_redraw();
            }
            Event::Window(window::Event::Focused) => {
                state.window_focused = true;
                shell.request_redraw();
            }
            Event::Keyboard(keyboard::Event::KeyPressed {
                key,
                modifiers,
                repeat,
                ..
            }) if state.focused
                && state.window_focused
                && self.on_press.is_some()
                && !shell.is_event_captured()
                && !modifiers.control()
                && !modifiers.logo()
                && !modifiers.alt() =>
            {
                if let Some(messages) = self
                    .on_key_press
                    .as_ref()
                    .and_then(|handler| handler(key, *modifiers))
                {
                    for message in messages {
                        shell.publish(message);
                    }
                    shell.capture_event();
                    shell.request_redraw();
                    return;
                }
                match key {
                    keyboard::Key::Named(keyboard::key::Named::Enter) => {
                        if !repeat {
                            shell.publish(self.on_press.as_ref().unwrap().clone());
                        }
                    }
                    keyboard::Key::Named(keyboard::key::Named::Space) => state.space_held = true,
                    keyboard::Key::Named(keyboard::key::Named::Escape) => state.unfocus(),
                    _ => {
                        self.inner.update(
                            &mut tree.children[0],
                            event,
                            layout,
                            cursor,
                            renderer,
                            clipboard,
                            shell,
                            viewport,
                        );
                        return;
                    }
                }
                shell.capture_event();
                shell.request_redraw();
                return;
            }
            Event::Keyboard(keyboard::Event::KeyReleased {
                key: keyboard::Key::Named(keyboard::key::Named::Space),
                ..
            }) if state.space_held && !shell.is_event_captured() => {
                state.space_held = false;
                if state.focused
                    && state.window_focused
                    && let Some(message) = &self.on_press
                {
                    shell.publish(message.clone());
                }
                shell.capture_event();
                shell.request_redraw();
                return;
            }
            _ => {}
        }
        self.inner.update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
    }
    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut R,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.inner.draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
        let state = tree.state.downcast_ref::<State<M>>();
        if state.focused && state.window_focused && self.on_press.is_some() {
            // Draw inside the button so focus remains visible in clipped bars.
            // The ring never changes its geometry or shifts adjacent controls.
            renderer.fill_quad(
                renderer::Quad {
                    bounds: layout.bounds(),
                    border: Border {
                        color: theme.extended_palette().primary.strong.color,
                        width: if motion::increase_contrast() {
                            3.0
                        } else {
                            2.0
                        },
                        radius: style::RADIUS_SMALL.into(),
                    },
                    ..renderer::Quad::default()
                },
                if state.space_held {
                    Color {
                        a: 0.12,
                        ..theme.extended_palette().primary.strong.color
                    }
                } else {
                    Color::TRANSPARENT
                },
            );
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
        self.inner
            .mouse_interaction(&tree.children[0], layout, cursor, viewport, renderer)
    }
    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &R,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, M, Theme, R>> {
        self.inner.overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}

impl<'a, M: Clone + 'static, R: renderer::Renderer + 'a> From<Button<'a, M, R>>
    for Element<'a, M, Theme, R>
{
    fn from(button: Button<'a, M, R>) -> Self {
        Element::new(button)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced::event;

    fn press(key: keyboard::key::Named, repeat: bool) -> Event {
        Event::Keyboard(keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(key),
            modified_key: keyboard::Key::Named(key),
            physical_key: keyboard::key::Physical::Unidentified(
                keyboard::key::NativeCode::Unidentified,
            ),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: None,
            repeat,
        })
    }
    fn release(key: keyboard::key::Named) -> Event {
        Event::Keyboard(keyboard::Event::KeyReleased {
            key: keyboard::Key::Named(key),
            modified_key: keyboard::Key::Named(key),
            physical_key: keyboard::key::Physical::Unidentified(
                keyboard::key::NativeCode::Unidentified,
            ),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
        })
    }
    #[test]
    fn keyboard_buttons_activate_once_and_capture_the_workbench_space_shortcut() {
        let mut button = Button::<_, ()>::new(iced::widget::Space::new()).on_press("activate");
        let mut tree = Tree::new(&button as &dyn Widget<&str, Theme, ()>);
        tree.state.downcast_mut::<State<&str>>().focus();
        let node = button.layout(
            &mut tree,
            &(),
            &layout::Limits::new(Size::ZERO, Size::new(100.0, 30.0)),
        );
        let viewport = Rectangle::with_size(node.size());
        let mut messages = Vec::new();
        let mut clipboard = iced::advanced::clipboard::Null;
        for event in [
            press(keyboard::key::Named::Space, false),
            press(keyboard::key::Named::Space, true),
        ] {
            let mut shell = Shell::new(&mut messages);
            button.update(
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
        }
        assert!(messages.is_empty(), "Space activates on release");
        button.update(
            &mut tree,
            &release(keyboard::key::Named::Space),
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &(),
            &mut clipboard,
            &mut Shell::new(&mut messages),
            &viewport,
        );
        assert_eq!(messages, ["activate"]);
        for event in [
            press(keyboard::key::Named::Enter, false),
            press(keyboard::key::Named::Enter, true),
        ] {
            button.update(
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
        assert_eq!(messages, ["activate", "activate"]);
    }
    #[test]
    fn disabled_buttons_are_excluded_from_focus_and_cancel_pending_activation() {
        let mut button = Button::<_, ()>::new(iced::widget::Space::new()).on_press("activate");
        let mut tree = Tree::new(&button as &dyn Widget<&str, Theme, ()>);
        let state = tree.state.downcast_mut::<State<&str>>();
        state.focus();
        state.space_held = true;
        button = button.on_press_maybe(None);
        button.diff(&mut tree);
        assert!(!tree.state.downcast_ref::<State<&str>>().is_focused());
        assert!(!tree.state.downcast_ref::<State<&str>>().space_held);
        let node = button.layout(
            &mut tree,
            &(),
            &layout::Limits::new(Size::ZERO, Size::new(100.0, 30.0)),
        );
        let mut count = iced::advanced::widget::operation::focusable::count();
        button.operate(
            &mut tree,
            Layout::new(&node),
            &(),
            &mut iced::advanced::widget::operation::black_box(&mut count),
        );
        let iced::advanced::widget::operation::Outcome::Some(count) = count.finish() else {
            panic!("count missing")
        };
        assert_eq!(count.total, 0);
    }

    #[test]
    fn a_checkbox_toggles_once_from_keyboard_or_pointer() {
        let mut button = Button::<_, ()>::new(
            iced::widget::checkbox(false)
                .size(16)
                .on_toggle(|value| value),
        )
        .padding(0)
        .style(style::bare)
        .on_press(true);
        let mut tree = Tree::new(&button as &dyn Widget<bool, Theme, ()>);
        tree.state.downcast_mut::<State<bool>>().focus();
        let node = button.layout(
            &mut tree,
            &(),
            &layout::Limits::new(Size::ZERO, Size::new(100.0, 30.0)),
        );
        let viewport = Rectangle::with_size(node.size());
        let mut messages = Vec::new();
        let mut clipboard = iced::advanced::clipboard::Null;
        for event in [
            press(keyboard::key::Named::Space, false),
            release(keyboard::key::Named::Space),
        ] {
            let mut shell = Shell::new(&mut messages);
            button.update(
                &mut tree,
                &event,
                Layout::new(&node),
                mouse::Cursor::Unavailable,
                &(),
                &mut clipboard,
                &mut shell,
                &viewport,
            );
            assert_eq!(
                shell.event_status(),
                event::Status::Captured,
                "Space belongs to the checkbox"
            );
        }
        assert_eq!(messages, [true]);
        for event in [
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
        ] {
            button.update(
                &mut tree,
                &event,
                Layout::new(&node),
                mouse::Cursor::Available(viewport.center()),
                &(),
                &mut clipboard,
                &mut Shell::new(&mut messages),
                &viewport,
            );
        }
        assert_eq!(
            messages,
            [true, true],
            "inner pointer handling does not also activate the wrapper"
        );
    }

    #[test]
    fn a_focused_compound_control_captures_navigation_and_commits_in_order() {
        let mut button = Button::<_, ()>::new(iced::widget::Space::new())
            .on_press("commit")
            .on_key_press(|key, _| {
                (key == &keyboard::Key::Named(keyboard::key::Named::ArrowRight))
                    .then(|| vec!["change", "commit"])
            });
        let mut tree = Tree::new(&button as &dyn Widget<&str, Theme, ()>);
        tree.state.downcast_mut::<State<&str>>().focus();
        let node = button.layout(
            &mut tree,
            &(),
            &layout::Limits::new(Size::ZERO, Size::new(100.0, 30.0)),
        );
        let viewport = Rectangle::with_size(node.size());
        let mut messages = Vec::new();
        let mut shell = Shell::new(&mut messages);
        button.update(
            &mut tree,
            &press(keyboard::key::Named::ArrowRight, false),
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &(),
            &mut iced::advanced::clipboard::Null,
            &mut shell,
            &viewport,
        );
        assert_eq!(shell.event_status(), event::Status::Captured);
        assert_eq!(messages, ["change", "commit"]);
    }

    #[test]
    fn semantic_control_keeps_its_identity_and_native_focus_across_view_diff() {
        let mut button = Button::<_, ()>::new(iced::widget::Space::new())
            .name("Include simulated cameras")
            .role(accessibility::Role::CheckBox)
            .checked(true)
            .on_press(false);
        let mut tree = Tree::new(&button as &dyn Widget<bool, Theme, ()>);
        let node = button.layout(
            &mut tree,
            &(),
            &layout::Limits::new(Size::ZERO, Size::new(100.0, 30.0)),
        );
        let (id, focus_id) = {
            let control = &tree.state.downcast_ref::<State<bool>>().control;
            (control.id, control.focus_id.clone())
        };
        button.operate(
            &mut tree,
            Layout::new(&node),
            &(),
            &mut iced::advanced::widget::operation::focusable::focus(focus_id),
        );
        let state = tree.state.downcast_ref::<State<bool>>();
        assert!(
            state.focused,
            "native focus uses the real keyboard focus ID"
        );
        assert_eq!(state.control.node.role(), accessibility::Role::CheckBox);
        assert_eq!(
            state.control.node.label(),
            Some("Include simulated cameras")
        );
        assert_eq!(
            state.control.node.toggled(),
            Some(accessibility::Toggled::True)
        );
        assert_eq!(state.control.activate, Some(false));

        button = button.checked(false).on_press_maybe(None);
        button.diff(&mut tree);
        button.operate(
            &mut tree,
            Layout::new(&node),
            &(),
            &mut iced::advanced::widget::operation::focusable::focus(Id::unique()),
        );
        let state = tree.state.downcast_ref::<State<bool>>();
        assert_eq!(state.control.id, id, "native nodes survive view rebuilding");
        assert_eq!(
            state.control.node.toggled(),
            Some(accessibility::Toggled::False)
        );
        assert!(state.control.node.is_disabled());
        assert!(state.control.activate.is_none());
    }
}
