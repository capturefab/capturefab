use super::*;
use iced::advanced::widget::operation;
use std::{any::Any, marker::PhantomData};

/// A tooltip supplies an accessible name for an otherwise unnamed icon or
/// compound button. Explicit control names are retained.
pub(in crate::gui) fn named<'a, M: Clone + 'static>(
    content: Element<'a, M>,
    name: String,
) -> Element<'a, M> {
    Element::new(Decorate::<M, M> {
        content,
        decoration: Decoration::Name(name),
        marker: PhantomData,
    })
}

/// `Element::map` translates event messages; this also translates semantic
/// actions collected from a child component's own message type.
pub(in crate::gui) fn mapped<'a, M: Clone + 'static>(
    content: Element<'a, M>,
    map: fn(M) -> Message,
) -> Element<'a, Message> {
    Element::new(Decorate::<M, Message> {
        content: content.map(map),
        decoration: Decoration::Map(map),
        marker: PhantomData,
    })
}

enum Decoration<M, N> {
    Name(String),
    Map(fn(M) -> N),
}
struct Decorate<'a, M, N> {
    content: Element<'a, N>,
    decoration: Decoration<M, N>,
    marker: PhantomData<fn(M)>,
}

struct DecoratedOperation<'a, M, N> {
    inner: &'a mut dyn Operation,
    decoration: &'a Decoration<M, N>,
}

impl<M: Clone + 'static, N: Clone + 'static> Operation for DecoratedOperation<'_, M, N> {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        operate(self);
    }
    fn container(&mut self, id: Option<&Id>, bounds: Rectangle) {
        self.inner.container(id, bounds);
    }
    fn focusable(
        &mut self,
        id: Option<&Id>,
        bounds: Rectangle,
        state: &mut dyn operation::Focusable,
    ) {
        self.inner.focusable(id, bounds, state);
    }
    fn text_input(
        &mut self,
        id: Option<&Id>,
        bounds: Rectangle,
        state: &mut dyn operation::TextInput,
    ) {
        self.inner.text_input(id, bounds, state);
    }
    fn text(&mut self, id: Option<&Id>, bounds: Rectangle, text: &str) {
        self.inner.text(id, bounds, text);
    }
    fn scrollable(
        &mut self,
        id: Option<&Id>,
        bounds: Rectangle,
        content: Rectangle,
        translation: Vector,
        state: &mut dyn operation::Scrollable,
    ) {
        self.inner
            .scrollable(id, bounds, content, translation, state);
    }
    fn custom(&mut self, id: Option<&Id>, bounds: Rectangle, state: &mut dyn Any) {
        let Some(control) = state.downcast_mut::<Control<M>>() else {
            self.inner.custom(id, bounds, state);
            return;
        };
        match self.decoration {
            Decoration::Name(name) => {
                if control
                    .node
                    .label()
                    .is_none_or(|label| matches!(label, "Button" | "Options" | ""))
                {
                    control.node.set_label(name.lines().next().unwrap_or(name));
                    if name.contains('\n') {
                        control.node.set_description(name.as_str());
                    }
                }
                self.inner.custom(id, bounds, control);
            }
            Decoration::Map(map) => {
                let mut mapped = Control::<N>::new(control.node.role());
                mapped.id = control.id;
                mapped.focus_id = control.focus_id.clone();
                mapped.node = control.node.clone();
                mapped.focused = control.focused;
                mapped.focusable = control.focusable;
                mapped.children = control.children.clone();
                mapped.activate = control.activate.clone().map(map);
                mapped.on_focus = control.on_focus.clone().map(map);
                mapped.increment = control.increment.clone().map(map);
                mapped.decrement = control.decrement.clone().map(map);
                mapped.set_value = control.set_value.as_ref().map(|handler| {
                    let handler = handler.clone();
                    let map = *map;
                    Arc::new(move |value| map(handler(value))) as ValueHandler<N>
                });
                mapped.replace_selected_text =
                    control.replace_selected_text.as_ref().map(|handler| {
                        let handler = handler.clone();
                        let map = *map;
                        Arc::new(move |value| map(handler(value))) as ValueHandler<N>
                    });
                mapped.native_input = control.native_input.clone();
                mapped.requested = control.requested.take();
                self.inner.custom(id, bounds, &mut mapped);
                control.requested = mapped.requested;
            }
        }
    }
}

impl<M: Clone + 'static, N: Clone + 'static> Widget<N, Theme, iced::Renderer>
    for Decorate<'_, M, N>
{
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
    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.content.as_widget_mut().layout(tree, renderer, limits)
    }
    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut iced::Renderer,
        theme: &Theme,
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
        renderer: &iced::Renderer,
        operation: &mut dyn Operation,
    ) {
        self.content.as_widget_mut().operate(
            tree,
            layout,
            renderer,
            &mut DecoratedOperation {
                inner: operation,
                decoration: &self.decoration,
            },
        );
    }
    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &iced::Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, N>,
        viewport: &Rectangle,
    ) {
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
        renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        self.content
            .as_widget()
            .mouse_interaction(tree, layout, cursor, viewport, renderer)
    }
    fn overlay<'a>(
        &'a mut self,
        tree: &'a mut Tree,
        layout: Layout<'a>,
        renderer: &iced::Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'a, N, Theme, iced::Renderer>> {
        self.content
            .as_widget_mut()
            .overlay(tree, layout, renderer, viewport, translation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn component_mapping_preserves_ids_values_and_widget_local_actions() {
        let mut control = Control::<crate::gui::destinations::Message>::new(Role::ComboBox);
        control.node.set_label("Save to");
        control.node.add_action(Action::Click);
        control.activate = Some(crate::gui::destinations::Message::OpenManager);
        control.on_focus = Some(crate::gui::destinations::Message::Output(
            "capture.png".into(),
        ));
        control.set_value = Some(Arc::new(crate::gui::destinations::Message::Output));
        let decoration = Decoration::Map(Message::CapturePicker);
        let mut collector = Collector::new(1.0);
        DecoratedOperation {
            inner: &mut collector,
            decoration: &decoration,
        }
        .custom(None, Rectangle::default(), &mut control);
        let route = &collector.routes[&control.id];
        assert!(matches!(
            route.activate,
            Some(Message::CapturePicker(
                crate::gui::destinations::Message::OpenManager
            ))
        ));
        assert!(
            matches!(route.set_value.as_ref().unwrap()("capture.png".into()), Message::CapturePicker(crate::gui::destinations::Message::Output(value)) if value == "capture.png")
        );
        assert!(matches!(
            &route.on_focus,
            Some(Message::CapturePicker(crate::gui::destinations::Message::Output(value)))
                if value == "capture.png"
        ));
        let action = ActionRequest {
            action: Action::Click,
            target: control.id,
            data: None,
        };
        let mut operation = WidgetAction(action.clone(), window::Id::unique());
        let mut erased = iced::advanced::widget::operation::black_box(&mut operation);
        DecoratedOperation {
            inner: &mut erased,
            decoration: &decoration,
        }
        .custom(None, Rectangle::default(), &mut control);
        assert_eq!(control.requested, Some(action));
    }

    #[test]
    fn tooltip_names_icon_buttons_and_preserves_explicit_names() {
        let mut control = Control::<Message>::new(Role::Button);
        let name = Decoration::<Message, Message>::Name("Close\nEscape".into());
        let mut collector = Collector::new(1.0);
        for fallback in [None, Some("Button"), Some("Options"), Some("")] {
            control.node = Node::new(Role::Button);
            if let Some(fallback) = fallback {
                control.node.set_label(fallback);
            }
            DecoratedOperation {
                inner: &mut collector,
                decoration: &name,
            }
            .custom(None, Rectangle::default(), &mut control);
            assert_eq!(control.node.label(), Some("Close"));
            assert_eq!(control.node.description(), Some("Close\nEscape"));
        }
        control.node.set_label("Close destination manager");
        DecoratedOperation {
            inner: &mut collector,
            decoration: &name,
        }
        .custom(None, Rectangle::default(), &mut control);
        assert_eq!(control.node.label(), Some("Close destination manager"));
    }
}
