//! Recover a control's focus after a temporary disabled state, without
//! stealing focus from a later keyboard, pointer or accessibility interaction.
use iced::advanced::{
    Layout, renderer,
    widget::{Id, Operation, Tree, operation},
};
use iced::{Element, Rectangle};

pub(in crate::gui) struct FocusReturn {
    pub id: Id,
    pub pending: bool,
    pub available: bool,
}

impl FocusReturn {
    pub fn new(id: Id) -> Self {
        Self {
            id,
            pending: false,
            available: false,
        }
    }
}

struct Cancel;
impl Operation for Cancel {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        operate(self);
    }
    fn custom(&mut self, _: Option<&Id>, _: Rectangle, state: &mut dyn std::any::Any) {
        if let Some(state) = state.downcast_mut::<FocusReturn>() {
            state.pending = false;
        }
    }
}

#[derive(Default)]
struct Scan {
    focused: bool,
    candidate: Option<Id>,
}
impl Operation for Scan {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        operate(self);
    }
    fn focusable(&mut self, _: Option<&Id>, _: Rectangle, state: &mut dyn operation::Focusable) {
        self.focused |= state.is_focused();
    }
    fn custom(&mut self, _: Option<&Id>, _: Rectangle, state: &mut dyn std::any::Any) {
        if let Some(state) = state.downcast_ref::<FocusReturn>()
            && state.pending
            && state.available
            && self.candidate.is_none()
        {
            self.candidate = Some(state.id.clone());
        }
    }
}

pub(super) fn cancel<M, T, R: renderer::Renderer>(
    content: &mut Element<'_, M, T, R>,
    tree: &mut Tree,
    layout: Layout<'_>,
    renderer: &R,
) {
    content
        .as_widget_mut()
        .operate(tree, layout, renderer, &mut Cancel);
}

pub(super) fn recover<M, T, R: renderer::Renderer>(
    content: &mut Element<'_, M, T, R>,
    tree: &mut Tree,
    layout: Layout<'_>,
    renderer: &R,
) -> bool {
    let mut scan = Scan::default();
    content
        .as_widget_mut()
        .operate(tree, layout, renderer, &mut scan);
    if scan.focused {
        cancel(content, tree, layout, renderer);
        return false;
    }
    let Some(candidate) = scan.candidate else {
        return false;
    };
    content.as_widget_mut().operate(
        tree,
        layout,
        renderer,
        &mut operation::focusable::focus::<()>(candidate),
    );
    cancel(content, tree, layout, renderer);
    true
}
