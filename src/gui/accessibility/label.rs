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
        description: None,
        id: None,
        role,
    })
}

pub(in crate::gui) fn label_description<'a, M: 'static>(
    content: Element<'a, M>,
    name: String,
    description: String,
    role: Role,
    id: Option<NodeId>,
) -> Element<'a, M> {
    Element::new(Label {
        content,
        name,
        description: Some(description),
        id,
        role,
    })
}

struct Label<'a, M, R: renderer::Renderer = iced::Renderer> {
    content: Element<'a, M, Theme, R>,
    name: String,
    description: Option<String>,
    id: Option<NodeId>,
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
        let original_id = control.id;
        if let Some(id) = self.id {
            control.id = id;
        }
        control.node = Node::new(self.role);
        control.node.set_label(self.name.as_str());
        if let Some(description) = &self.description {
            control.node.set_description(description.as_str());
        }
        control.focusable = false;
        match self.role {
            Role::Alert => {
                // The macOS adapter announces the consumer's text value. A
                // label alone names the region but does not produce an event.
                control.node.set_value(self.name.as_str());
                control.node.set_live(accesskit::Live::Assertive);
                control.node.set_live_atomic();
            }
            Role::Status => {
                control.node.set_value(self.name.as_str());
                control.node.set_live(accesskit::Live::Polite);
                control.node.set_live_atomic();
            }
            Role::Heading => control.node.set_level(2),
            _ => {}
        }
        operation.custom(None, layout.bounds(), control);
        control.id = original_id;
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
    fn explicit_label_identity_does_not_leak_when_its_widget_state_is_reused() {
        let first = new_node_id();
        let second = new_node_id();
        let mut label = Label {
            description: Some("Full measurement context".into()),
            id: Some(first),
            content: iced::widget::Text::<Theme, ()>::new("Measurement").into(),
            name: "Measurement".into(),
            role: Role::Label,
        };
        let mut tree = Tree::new(&label as &dyn Widget<Message, Theme, ()>);
        let original = tree.state.downcast_ref::<Control<Message>>().id;
        for (provided, expected) in [
            (Some(first), first),
            (Some(second), second),
            (None, original),
        ] {
            label.id = provided;
            label.diff(&mut tree);
            let node = label.layout(
                &mut tree,
                &(),
                &layout::Limits::new(Size::ZERO, Size::new(300.0, 50.0)),
            );
            let mut collector = Collector::new(1.0);
            label.operate(&mut tree, Layout::new(&node), &(), &mut collector);
            assert_eq!(collector.nodes.len(), 1);
            assert_eq!(collector.nodes[0].0, expected);
            assert_eq!(tree.state.downcast_ref::<Control<Message>>().id, original);
        }
    }

    #[test]
    fn error_live_regions_keep_their_identity_without_duplicate_text_or_focus() {
        let mut label = Label {
            description: None,
            id: None,
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
        assert_eq!(alert.value(), Some("Connection failed"));
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

    #[derive(Default)]
    struct ValueChanges(Vec<(NodeId, Option<String>, Option<String>, accesskit::Live)>);

    impl accesskit_consumer::TreeChangeHandler for ValueChanges {
        fn node_added(&mut self, _: &accesskit_consumer::Node<'_>) {}
        fn node_removed(&mut self, _: &accesskit_consumer::Node<'_>) {}
        fn focus_moved(
            &mut self,
            _: Option<&accesskit_consumer::Node<'_>>,
            _: Option<&accesskit_consumer::Node<'_>>,
        ) {
        }
        fn node_updated(
            &mut self,
            old: &accesskit_consumer::Node<'_>,
            new: &accesskit_consumer::Node<'_>,
        ) {
            if matches!(new.role(), Role::Alert | Role::Status) && old.value() != new.value() {
                self.0
                    .push((new.id(), old.value(), new.value(), new.live()));
            }
        }
    }

    #[test]
    fn actual_live_widgets_supply_native_consumer_values_once_per_changed_outcome() {
        let renderer = crate::gui::destinations::layout_tests::renderer();
        for (role, live, initial, completed) in [
            (
                Role::Status,
                accesskit::Live::Polite,
                "Checking connections and applying defaults",
                "Network check finished: one camera needs attention",
            ),
            (
                Role::Alert,
                accesskit::Live::Assertive,
                "Capture failed: output file already exists",
                "Capture failed: choose a new output file name",
            ),
        ] {
            let view = |name: &str| {
                label(
                    iced::widget::text(name.to_owned()).into(),
                    name.to_owned(),
                    role,
                )
            };
            let mut widget_tree = Tree::empty();
            let mut collect = |name| {
                let mut view: Element<'_, Message> = view(name);
                widget_tree.diff(view.as_widget());
                let layout = view.as_widget_mut().layout(
                    &mut widget_tree,
                    &renderer,
                    &layout::Limits::new(Size::ZERO, Size::new(420.0, 80.0)),
                );
                snapshot_widget(
                    view.as_widget_mut(),
                    &mut widget_tree,
                    Layout::new(&layout),
                    &renderer,
                )
                .0
            };
            let initial_tree = collect(initial);
            let id = initial_tree
                .nodes
                .iter()
                .find(|(_, node)| node.role() == role)
                .unwrap()
                .0;
            assert_eq!(initial_tree.nodes.len(), 2, "one region plus window root");
            let mut consumer = accesskit_consumer::Tree::new(initial_tree, false);
            let region = consumer.state().node_by_id(id).unwrap();
            assert_eq!(region.label().as_deref(), Some(initial));
            assert_eq!(region.value().as_deref(), Some(initial));
            assert_eq!(region.live(), live);
            assert!(!region.is_focused(), "live text does not take focus");

            let mut changes = ValueChanges::default();
            let completed_tree = collect(completed);
            assert_eq!(completed_tree.nodes.len(), 2, "no duplicate static caption");
            consumer.update_and_process_changes(completed_tree, &mut changes);
            assert_eq!(
                changes.0,
                [(id, Some(initial.into()), Some(completed.into()), live)],
                "the pinned native consumer receives the changed announcement value"
            );
            let region = consumer.state().node_by_id(id).unwrap();
            assert_eq!(region.label().as_deref(), Some(completed));
            assert_eq!(region.value().as_deref(), Some(completed));
            consumer.update_and_process_changes(collect(completed), &mut changes);
            assert_eq!(changes.0.len(), 1, "unchanged redraws do not repeat values");
        }
    }
}
