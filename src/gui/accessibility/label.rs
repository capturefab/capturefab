use super::*;

/// Static meaning with a stable widget-state ID. Alerts and status changes use
/// native live-region events; camera frame changes themselves are never live.
pub(in crate::gui) fn label<'a, M: 'static>(
    content: Element<'a, M>,
    name: String,
    role: Role,
) -> Element<'a, M> {
    Element::new(Label {
        content,
        name,
        role,
    })
}

struct Label<'a, M, R: renderer::Renderer = iced::Renderer> {
    content: Element<'a, M, Theme, R>,
    name: String,
    role: Role,
}

impl<M: 'static, R: renderer::Renderer> Widget<M, Theme, R> for Label<'_, M, R> {
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
        control.focusable = false;
        match self.role {
            Role::Alert => {
                control.node.set_live(accesskit::Live::Assertive);
                control.node.set_live_atomic();
            }
            Role::Status => {
                control.node.set_live(accesskit::Live::Polite);
                control.node.set_live_atomic();
            }
            Role::Heading => control.node.set_level(2),
            _ => {}
        }
        operation.custom(None, layout.bounds(), control);
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

    #[test]
    fn error_live_regions_keep_their_identity_without_duplicate_text_or_focus() {
        let mut label = Label {
            content: iced::widget::Text::<Theme, ()>::new("Connection failed").into(),
            name: "Connection failed".into(),
            role: Role::Alert,
        };
        let mut tree = Tree::new(&label as &dyn Widget<Message, Theme, ()>);
        let node = label.layout(
            &mut tree,
            &(),
            &layout::Limits::new(Size::ZERO, Size::new(300.0, 50.0)),
        );
        let id = tree.state.downcast_ref::<Control<Message>>().id;
        let mut collector = Collector::new(1.0);
        label.operate(&mut tree, Layout::new(&node), &(), &mut collector);
        assert_eq!(
            collector.nodes.len(),
            1,
            "visible error text belongs to its live region"
        );
        let alert = &collector.nodes[0].1;
        assert_eq!(alert.role(), Role::Alert);
        assert_eq!(alert.label(), Some("Connection failed"));
        assert_eq!(alert.live(), Some(accesskit::Live::Assertive));
        assert!(!alert.supports_action(Action::Focus));
        label.name = "Connection failed: check the camera address".into();
        label.diff(&mut tree);
        let mut collector = Collector::new(1.0);
        label.operate(&mut tree, Layout::new(&node), &(), &mut collector);
        assert_eq!(collector.nodes[0].0, id);
        assert!(
            collector.nodes[0]
                .1
                .label()
                .unwrap()
                .contains("check the camera address")
        );
    }
}
