//! Fallback scrolling for sheets and focused panes, after children handle keys.
use iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, overlay, renderer,
    widget::{
        Id, Operation, Tree,
        operation::{self, Scrollable},
        tree,
    },
};
use iced::widget::scrollable::Anchor;
use iced::{Element, Event, Length, Rectangle, Size, Vector, keyboard, mouse, window};

pub(super) fn wrap<'a, M: 'a, T: 'a, R: renderer::Renderer + 'a>(
    content: Element<'a, M, T, R>,
    target: &'static str,
) -> Element<'a, M, T, R> {
    Element::new(KeyboardScroll {
        content,
        target: Id::new(target),
        focused_only: false,
        vertical_anchor: Anchor::Start,
    })
}

/// A nonmodal pane scrolls only while one of its actual controls has focus.
/// This keeps its keys from taking over an unrelated editor or camera pane.
pub(super) fn panel<'a, M: 'a, T: 'a, R: renderer::Renderer + 'a>(
    content: Element<'a, M, T, R>,
    target: &'static str,
    vertical_anchor: Anchor,
) -> Element<'a, M, T, R> {
    Element::new(KeyboardScroll {
        content,
        target: Id::new(target),
        focused_only: true,
        vertical_anchor,
    })
}

struct KeyboardScroll<'a, M, T, R> {
    content: Element<'a, M, T, R>,
    target: Id,
    focused_only: bool,
    vertical_anchor: Anchor,
}
struct State {
    window_focused: bool,
}
impl Default for State {
    fn default() -> Self {
        Self {
            window_focused: true,
        }
    }
}
struct Scroll {
    target: Id,
    key: keyboard::key::Named,
    accepted: bool,
    changed: bool,
    vertical_anchor: Anchor,
}
impl Operation for Scroll {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        operate(self);
    }
    fn scrollable(
        &mut self,
        id: Option<&Id>,
        bounds: Rectangle,
        content: Rectangle,
        translation: Vector,
        state: &mut dyn Scrollable,
    ) {
        if id != Some(&self.target) {
            return;
        }
        use keyboard::key::Named;
        let max = Vector::new(
            (content.width - bounds.width).max(0.0),
            (content.height - bounds.height).max(0.0),
        );
        let mut offset = translation;
        let step = 32.0;
        match self.key {
            Named::ArrowDown if max.y > 0.0 => offset.y += step,
            Named::ArrowUp if max.y > 0.0 => offset.y -= step,
            Named::PageDown if max.y > 0.0 => offset.y += (bounds.height - step).max(step),
            Named::PageUp if max.y > 0.0 => offset.y -= (bounds.height - step).max(step),
            Named::ArrowRight if max.x > 0.0 => offset.x += step,
            Named::ArrowLeft if max.x > 0.0 => offset.x -= step,
            Named::Home if max.y > 0.0 => offset.y = 0.0,
            Named::End if max.y > 0.0 => offset.y = max.y,
            Named::Home if max.x > 0.0 => offset.x = 0.0,
            Named::End if max.x > 0.0 => offset.x = max.x,
            _ => return,
        }
        offset.x = offset.x.clamp(0.0, max.x);
        offset.y = offset.y.clamp(0.0, max.y);
        self.accepted = true;
        self.changed = offset != translation;
        state.scroll_to(operation::scrollable::AbsoluteOffset {
            x: Some(offset.x),
            y: Some(match self.vertical_anchor {
                Anchor::Start => offset.y,
                Anchor::End => max.y - offset.y,
            }),
        });
    }
}

#[derive(Default)]
struct Focus(bool);
impl Operation for Focus {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        operate(self);
    }
    fn focusable(&mut self, _: Option<&Id>, _: Rectangle, state: &mut dyn operation::Focusable) {
        self.0 |= state.is_focused();
    }
}

impl<M, T, R: renderer::Renderer> Widget<M, T, R> for KeyboardScroll<'_, M, T, R> {
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
            cursor,
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
        self.content
            .as_widget_mut()
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
        let state = tree.state.downcast_mut::<State>();
        match event {
            Event::Window(window::Event::Focused) => state.window_focused = true,
            Event::Window(window::Event::Unfocused) => state.window_focused = false,
            _ => {}
        }
        let window_focused = state.window_focused;
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
        if !window_focused || shell.is_event_captured() {
            return;
        }
        let Event::Keyboard(keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(key),
            modifiers,
            ..
        }) = event
        else {
            return;
        };
        if !modifiers.is_empty()
            || !matches!(
                key,
                keyboard::key::Named::ArrowUp
                    | keyboard::key::Named::ArrowDown
                    | keyboard::key::Named::ArrowLeft
                    | keyboard::key::Named::ArrowRight
                    | keyboard::key::Named::PageUp
                    | keyboard::key::Named::PageDown
                    | keyboard::key::Named::Home
                    | keyboard::key::Named::End
            )
        {
            return;
        }
        if self.focused_only {
            let mut focused = Focus::default();
            self.content.as_widget_mut().operate(
                &mut tree.children[0],
                layout,
                renderer,
                &mut focused,
            );
            if !focused.0 {
                return;
            }
        }
        let mut scroll = Scroll {
            target: self.target.clone(),
            key: *key,
            accepted: false,
            changed: false,
            vertical_anchor: self.vertical_anchor,
        };
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, &mut scroll);
        if scroll.accepted {
            shell.capture_event();
        }
        if scroll.changed {
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
        self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
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
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}
