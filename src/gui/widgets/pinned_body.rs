//! Reserve real pinned-header/footer geometry before a shrinking scroll body.
use iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, overlay, renderer,
    widget::{Operation, Tree},
};
use iced::{Element, Event, Length, Rectangle, Size, Theme, Vector, mouse};

pub(super) fn wrap<'a, M: 'a>(content: Element<'a, M>) -> Element<'a, M> {
    Element::new(Body(content, Allocation::RemainingBody))
}

pub(super) fn column<'a, M: 'a>(content: Element<'a, M>) -> Element<'a, M> {
    Element::new(Body(content, Allocation::NaturalColumn))
}

#[derive(Clone, Copy)]
enum Allocation {
    RemainingBody,
    NaturalColumn,
}

struct Body<'a, M>(Element<'a, M>, Allocation);

impl<M> Widget<M, Theme, iced::Renderer> for Body<'_, M> {
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(self.0.as_widget())]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(&[self.0.as_widget()]);
    }

    fn size(&self) -> Size<Length> {
        // Column lays out non-fluid header/footer children before this slot.
        Size::new(
            self.0.as_widget().size().width,
            if matches!(self.1, Allocation::NaturalColumn) {
                Length::Shrink
            } else {
                Length::Fill
            },
        )
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        // Remove Fill's minimum: short forms should retain their natural
        // height, while tall forms scroll within the remaining maximum.
        let limits = if matches!(self.1, Allocation::NaturalColumn) {
            // Allocate Column's fixed children first, then trim its final
            // frame to their measured height instead of filling the window.
            layout::Limits::with_compression(
                Size::ZERO,
                limits.max(),
                Size::new(limits.compression().width, false),
            )
        } else {
            limits.loose()
        };
        let node = self
            .0
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, &limits);
        if matches!(self.1, Allocation::NaturalColumn) {
            let height = node
                .children()
                .iter()
                .map(|child| child.bounds().y + child.size().height)
                .fold(0.0_f32, f32::max);
            layout::Node::with_children(
                Size::new(node.size().width, height.min(node.size().height)),
                node.children().to_vec(),
            )
        } else {
            node
        }
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &iced::Renderer,
        operation: &mut dyn Operation,
    ) {
        self.0
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &iced::Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, M>,
        viewport: &Rectangle,
    ) {
        self.0.as_widget_mut().update(
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
        renderer: &mut iced::Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.0.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
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
        self.0
            .as_widget()
            .mouse_interaction(&tree.children[0], layout, cursor, viewport, renderer)
    }

    fn overlay<'a>(
        &'a mut self,
        tree: &'a mut Tree,
        layout: Layout<'a>,
        renderer: &iced::Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'a, M, Theme, iced::Renderer>> {
        self.0.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}
