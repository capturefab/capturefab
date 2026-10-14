//! Named native groups retain the actual laid-out semantic descendants.
use super::*;

pub(in crate::gui) fn group<'a, M: 'static>(
    content: Element<'a, M>,
    name: String,
    role: Role,
) -> Element<'a, M> {
    Element::new(Group {
        content,
        name,
        role,
    })
}

struct Group<'a, M, R: renderer::Renderer = iced::Renderer> {
    content: Element<'a, M, Theme, R>,
    name: String,
    role: Role,
}

pub(super) struct Marker {
    pub id: NodeId,
    pub begin: bool,
}

pub(in crate::gui) fn scope(
    operation: &mut dyn Operation,
    bounds: Rectangle,
    id: NodeId,
    begin: bool,
) {
    operation.custom(None, bounds, &mut Marker { id, begin });
}

impl<M: 'static, R: renderer::Renderer> Widget<M, Theme, R> for Group<'_, M, R> {
    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }
    fn size_hint(&self) -> Size<Length> {
        self.content.as_widget().size_hint()
    }
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<Control<M>>()
    }
    fn state(&self) -> tree::State {
        tree::State::new(Control::<M>::new(self.role))
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
        theme: &Theme,
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
        let control = tree.state.downcast_mut::<Control<M>>();
        control.node = Node::new(self.role);
        control.node.set_label(self.name.as_str());
        if self.role == Role::AlertDialog {
            control.node.set_modal();
        }
        control.focusable = false;
        operation.custom(None, layout.bounds(), control);
        scope(operation, layout.bounds(), control.id, true);
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
        scope(operation, layout.bounds(), control.id, false);
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
    ) -> Option<overlay::Element<'a, M, Theme, R>> {
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gui::button::Button;

    #[test]
    fn dialog_groups_actual_controls_static_text_and_nested_children_once() {
        let nested = Group {
            content: iced::widget::Text::<Theme, ()>::new(
                "Save failed. Check the folder permissions.",
            )
            .into(),
            name: "Failure details".into(),
            role: Role::Group,
        };
        let content = iced::widget::Column::new().push(Element::new(nested)).push(
            Button::<_, ()>::new(iced::widget::Space::new().width(80).height(24))
                .name("Retry saving")
                .on_press(Message::Discover),
        );
        let mut dialog = Group {
            content: content.into(),
            name: "Couldn't save preferences".into(),
            role: Role::AlertDialog,
        };
        let mut tree = Tree::new(&dialog as &dyn Widget<Message, Theme, ()>);
        let node = dialog.layout(
            &mut tree,
            &(),
            &layout::Limits::new(Size::ZERO, Size::new(300.0, 120.0)),
        );
        let (snapshot, _, _) = snapshot_widget(&mut dialog, &mut tree, Layout::new(&node), &());
        let nodes: std::collections::HashMap<_, _> = snapshot.nodes.into_iter().collect();
        let (dialog_id, dialog_node) = nodes
            .iter()
            .find(|(_, node)| node.role() == Role::AlertDialog)
            .unwrap();
        let (group_id, group_node) = nodes
            .iter()
            .find(|(_, node)| node.role() == Role::Group)
            .unwrap();
        let (label_id, _) = nodes
            .iter()
            .find(|(_, node)| node.role() == Role::Label)
            .unwrap();
        let (button_id, _) = nodes
            .iter()
            .find(|(_, node)| node.role() == Role::Button)
            .unwrap();
        assert!(dialog_node.is_modal());
        assert!(!dialog_node.supports_action(accesskit::Action::Focus));
        assert_eq!(dialog_node.children(), [*group_id, *button_id]);
        assert_eq!(group_node.children(), [*label_id]);
        assert_eq!(nodes[&ROOT].children(), [*dialog_id]);
        assert_eq!(
            nodes
                .values()
                .filter(|node| node.children().contains(label_id))
                .count(),
            1
        );
        let id_before = *dialog_id;
        let (updated, _, _) = snapshot_widget(&mut dialog, &mut tree, Layout::new(&node), &());
        assert!(
            updated
                .nodes
                .iter()
                .any(|(id, node)| *id == id_before && node.role() == Role::AlertDialog)
        );
    }
}
