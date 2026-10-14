//! Selection emphasis follows real widget focus or the last document pointer
//! target, without changing caret, selection, menu or modal focus.
use super::*;
#[cfg(test)]
mod tests;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub(in crate::gui) enum Area {
    #[default]
    Image,
    CameraList,
    Other,
}

#[derive(Default)]
pub(super) struct Tracker {
    pub pointer_hint: Area,
    pub reported: Option<Area>,
}

pub(in crate::gui) fn scope<'a, M: 'a, T: 'a, R: renderer::Renderer + 'a>(
    content: Element<'a, M, T, R>,
    area: Area,
) -> Element<'a, M, T, R> {
    Element::new(Scope { content, area })
}

struct Scope<'a, M, T, R> {
    content: Element<'a, M, T, R>,
    area: Area,
}
#[derive(Default)]
struct ScopeState {
    pressed: bool,
}
struct Marker {
    area: Area,
    begin: bool,
    pressed: bool,
}

pub(super) fn pointer_position(event: &Event, cursor: mouse::Cursor) -> Option<iced::Point> {
    match event {
        Event::Mouse(mouse::Event::ButtonPressed(_)) => cursor.position(),
        Event::Touch(iced::touch::Event::FingerPressed { position, .. }) => Some(*position),
        _ => None,
    }
}

impl<M, T, R: renderer::Renderer> Widget<M, T, R> for Scope<'_, M, T, R> {
    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }
    fn size_hint(&self) -> Size<Length> {
        self.content.as_widget().size_hint()
    }
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<ScopeState>()
    }
    fn state(&self) -> tree::State {
        tree::State::new(ScopeState::default())
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
        let state = tree.state.downcast_mut::<ScopeState>();
        let mut marker = Marker {
            area: self.area,
            begin: true,
            pressed: state.pressed,
        };
        operation.custom(None, layout.bounds(), &mut marker);
        state.pressed = marker.pressed;
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
        operation.custom(
            None,
            layout.bounds(),
            &mut Marker {
                area: self.area,
                begin: false,
                pressed: false,
            },
        );
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
        // A floating unscoped control may cover this content. Only a delivered,
        // still-unclaimed visible press identifies its document target.
        if !shell.is_event_captured()
            && let Some(position) = pointer_position(event, cursor)
            && layout.bounds().contains(position)
            && viewport.contains(position)
        {
            tree.state.downcast_mut::<ScopeState>().pressed = true;
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

#[derive(Default)]
struct Scan {
    scopes: Vec<Area>,
    focused: Option<Area>,
    pointer: Option<Area>,
    clear_pointer: bool,
}
impl Operation for Scan {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        operate(self);
    }
    fn focusable(&mut self, _: Option<&Id>, _: Rectangle, state: &mut dyn operation::Focusable) {
        if state.is_focused() && self.focused.is_none() {
            self.focused = Some(self.scopes.last().copied().unwrap_or(Area::Other));
        }
    }
    fn custom(&mut self, _: Option<&Id>, _: Rectangle, state: &mut dyn std::any::Any) {
        if let Some(marker) = state.downcast_mut::<Marker>() {
            if marker.begin {
                if self.clear_pointer {
                    marker.pressed = false;
                }
                if marker.pressed {
                    self.pointer = Some(marker.area);
                }
                self.scopes.push(marker.area);
            } else {
                self.scopes.pop();
            }
        }
    }
}

fn scan<M, T, R: renderer::Renderer>(
    content: &mut Element<'_, M, T, R>,
    tree: &mut Tree,
    layout: Layout<'_>,
    renderer: &R,
    clear_pointer: bool,
) -> Scan {
    let mut scan = Scan {
        clear_pointer,
        ..Scan::default()
    };
    content
        .as_widget_mut()
        .operate(tree, layout, renderer, &mut scan);
    scan
}
pub(super) fn clear_pointer<M, T, R: renderer::Renderer>(
    content: &mut Element<'_, M, T, R>,
    tree: &mut Tree,
    layout: Layout<'_>,
    renderer: &R,
) {
    scan(content, tree, layout, renderer, true);
}
pub(super) fn pointer_area<M, T, R: renderer::Renderer>(
    content: &mut Element<'_, M, T, R>,
    tree: &mut Tree,
    layout: Layout<'_>,
    renderer: &R,
) -> Option<Area> {
    scan(content, tree, layout, renderer, false).pointer
}
pub(super) fn focused_area<M, T, R: renderer::Renderer>(
    content: &mut Element<'_, M, T, R>,
    tree: &mut Tree,
    layout: Layout<'_>,
    renderer: &R,
) -> Option<Area> {
    scan(content, tree, layout, renderer, false).focused
}
