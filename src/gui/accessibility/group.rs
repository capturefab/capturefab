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
        id: None,
        modal: role == Role::AlertDialog,
    })
}

pub(in crate::gui) fn modal_group<'a, M: 'static>(
    content: Element<'a, M>,
    name: String,
    role: Role,
) -> Element<'a, M> {
    Element::new(Group {
        content,
        name,
        role,
        id: None,
        modal: true,
    })
}

/// A journal-owned ID preserves a noninteractive group's identity when the
/// widget toolkit reorders or reconstructs its children. Callers allocate from
/// the shared allocator and retain IDs only while the journal entry exists.
pub(in crate::gui) fn group_with_id<'a, M: 'static>(
    content: Element<'a, M>,
    name: String,
    id: NodeId,
) -> Element<'a, M> {
    Element::new(Group {
        content,
        name,
        role: Role::Group,
        id: Some(id),
        modal: false,
    })
}

struct Group<'a, M, R: renderer::Renderer = iced::Renderer> {
    content: Element<'a, M, Theme, R>,
    name: String,
    role: Role,
    id: Option<NodeId>,
    modal: bool,
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
        let state_id = control.id;
        if let Some(id) = self.id {
            control.id = id;
        }
        control.node = Node::new(self.role);
        control.node.set_label(self.name.as_str());
        if self.modal {
            control.node.set_modal();
        }
        control.focusable = false;
        operation.custom(None, layout.bounds(), control);
        scope(operation, layout.bounds(), control.id, true);
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
        scope(operation, layout.bounds(), control.id, false);
        // The optional journal identity belongs only to this view. Preserve
        // the widget state's own allocation if the same tree is later reused
        // for an ordinary group or a different journal entry.
        control.id = state_id;
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
            id: None,
            modal: false,
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
            id: None,
            modal: true,
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

    #[test]
    fn replacing_an_explicit_journal_group_with_an_ordinary_group_restores_its_own_identity() {
        let explicit = new_node_id();
        let mut group = Group {
            content: iced::widget::Text::<Theme, ()>::new("Command details").into(),
            name: "Command 1".into(),
            role: Role::Group,
            id: Some(explicit),
            modal: false,
        };
        let mut tree = Tree::new(&group as &dyn Widget<Message, Theme, ()>);
        let own_id = tree.state.downcast_ref::<Control<Message>>().id;
        assert_ne!(own_id, explicit);
        let layout = group.layout(
            &mut tree,
            &(),
            &layout::Limits::new(Size::ZERO, Size::new(300.0, 100.0)),
        );
        let (journal, _, _) = snapshot_widget(&mut group, &mut tree, Layout::new(&layout), &());
        assert!(
            journal
                .nodes
                .iter()
                .any(|(id, node)| *id == explicit && node.label() == Some("Command 1"))
        );
        assert_eq!(tree.state.downcast_ref::<Control<Message>>().id, own_id);
        group.id = None;
        group.name = "Session details".into();
        group.diff(&mut tree);
        let (ordinary, bridge, window) =
            snapshot_widget(&mut group, &mut tree, Layout::new(&layout), &());
        assert!(
            ordinary
                .nodes
                .iter()
                .any(|(id, node)| *id == own_id && node.label() == Some("Session details"))
        );
        assert!(!ordinary.nodes.iter().any(|(id, _)| *id == explicit));
        for action in [Action::Focus, Action::Click] {
            assert!(
                iced_runtime::task::into_stream(bridge.dispatch(Request {
                    window,
                    replay: None,
                    action: ActionRequest {
                        action,
                        target: explicit,
                        data: None
                    },
                }))
                .is_none()
            );
        }
    }
}
