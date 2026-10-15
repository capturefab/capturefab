//! Semantics for the rendered desktop menu, preserving document editor focus.
use super::*;
use crate::gui::accessibility;
use iced::advanced::widget::Id;
use std::sync::{Arc, Mutex};

pub(super) type HeaderIds = Arc<Mutex<Vec<accessibility::NodeId>>>;

pub(super) struct Region<'a, R: renderer::Renderer = iced::Renderer> {
    pub content: Element<'a, Message, Theme, R>,
    pub role: accessibility::Role,
    pub name: String,
    pub focus: Option<usize>,
    pub override_focus: bool,
    pub inputs: Vec<Input>,
    pub shortcuts: Vec<Option<String>>,
    pub headers: HeaderIds,
    pub popup_for: Option<usize>,
}

fn is_item(role: accessibility::Role) -> bool {
    matches!(
        role,
        accessibility::Role::MenuItem
            | accessibility::Role::MenuItemCheckBox
            | accessibility::Role::MenuItemRadio
    )
}

struct Items<'a> {
    inner: &'a mut dyn Operation,
    role: accessibility::Role,
    focus: Option<usize>,
    override_focus: bool,
    inputs: &'a [Input],
    shortcuts: &'a [Option<String>],
    index: usize,
    headers: &'a HeaderIds,
}
impl Operation for Items<'_> {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        let index = &mut self.index;
        let (role, focus, override_focus, inputs, shortcuts, headers) = (
            self.role,
            self.focus,
            self.override_focus,
            self.inputs,
            self.shortcuts,
            self.headers,
        );
        self.inner.traverse(&mut |inner| {
            let mut items = Items {
                inner,
                role,
                focus,
                override_focus,
                inputs,
                shortcuts,
                index: *index,
                headers,
            };
            operate(&mut items);
            *index = items.index;
        });
    }
    fn container(&mut self, id: Option<&Id>, bounds: Rectangle) {
        self.inner.container(id, bounds);
    }
    fn focusable(
        &mut self,
        id: Option<&Id>,
        bounds: Rectangle,
        state: &mut dyn iced::advanced::widget::operation::Focusable,
    ) {
        self.inner.focusable(id, bounds, state);
    }
    fn text_input(
        &mut self,
        id: Option<&Id>,
        bounds: Rectangle,
        state: &mut dyn iced::advanced::widget::operation::TextInput,
    ) {
        self.inner.text_input(id, bounds, state);
    }
    fn scrollable(
        &mut self,
        id: Option<&Id>,
        bounds: Rectangle,
        content: Rectangle,
        translation: Vector,
        state: &mut dyn iced::advanced::widget::operation::Scrollable,
    ) {
        self.inner
            .scrollable(id, bounds, content, translation, state);
    }
    fn text(&mut self, id: Option<&Id>, bounds: Rectangle, text: &str) {
        self.inner.text(id, bounds, text);
    }
    fn custom(&mut self, id: Option<&Id>, bounds: Rectangle, state: &mut dyn std::any::Any) {
        if let Some(control) = state.downcast_mut::<accessibility::Control<Message>>()
            && is_item(control.node.role())
        {
            if self.override_focus {
                control.focused = self.focus == Some(self.index);
            }
            control.on_focus = self
                .inputs
                .get(self.index)
                .copied()
                .map(Message::DesktopMenu);
            // Menu navigation has logical focus in DesktopState while the
            // document editor retains its caret/selection for Edit commands.
            // It advertises native Focus without routing through widget focus.
            let widget_focusable = control.focusable;
            control.focusable = false;
            if !control.node.is_disabled() {
                control.node.add_action(accesskit::Action::Focus);
            }
            if let Some(shortcut) = self.shortcuts.get(self.index).and_then(Option::as_ref) {
                control.node.set_keyboard_shortcut(shortcut.as_str());
            }
            if let Some(letter) = control.node.label().and_then(first_letter) {
                control.node.set_access_key(letter.to_string());
            }
            if self.role == accessibility::Role::MenuBar {
                control.node.set_has_popup(accesskit::HasPopup::Menu);
            }
            if self.role == accessibility::Role::MenuBar {
                self.headers.lock().unwrap().push(control.id);
            }
            self.index += 1;
            self.inner.custom(id, bounds, control);
            control.focusable = widget_focusable;
            return;
        }
        self.inner.custom(id, bounds, state);
    }
}

impl<R: renderer::Renderer> Widget<Message, Theme, R> for Region<'_, R> {
    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }
    fn size_hint(&self) -> Size<Length> {
        self.content.as_widget().size_hint()
    }
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<accessibility::Control<Message>>()
    }
    fn state(&self) -> tree::State {
        tree::State::new(accessibility::Control::<Message>::new(self.role))
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
        let control = tree.state.downcast_mut::<accessibility::Control<Message>>();
        control.node = accessibility::Node::new(self.role);
        control.node.set_label(self.name.as_str());
        control
            .node
            .set_orientation(if self.role == accessibility::Role::MenuBar {
                accesskit::Orientation::Horizontal
            } else {
                accesskit::Orientation::Vertical
            });
        control.focusable = false;
        if self.role == accessibility::Role::MenuBar {
            self.headers.lock().unwrap().clear();
        } else if let Some(header) = self
            .popup_for
            .and_then(|index| self.headers.lock().unwrap().get(index).copied())
        {
            control.node.set_popup_for(header);
        }
        operation.custom(None, layout.bounds(), control);
        accessibility::scope(operation, layout.bounds(), control.id, true);
        let mut decorate = Items {
            inner: operation,
            index: 0,
            role: self.role,
            focus: self.focus,
            override_focus: self.override_focus,
            inputs: &self.inputs,
            shortcuts: &self.shortcuts,
            headers: &self.headers,
        };
        self.content.as_widget_mut().operate(
            &mut tree.children[0],
            layout,
            renderer,
            &mut decorate,
        );
        accessibility::scope(operation, layout.bounds(), control.id, false);
    }
    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &R,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
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
    ) -> Option<overlay::Element<'a, Message, Theme, R>> {
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
    use iced::widget::{Column, Row, Space};

    #[derive(Default)]
    struct Routes(Vec<(accessibility::NodeId, Option<Message>, Option<Message>)>);
    impl Operation for Routes {
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
            operate(self);
        }
        fn custom(&mut self, _: Option<&Id>, _: Rectangle, state: &mut dyn std::any::Any) {
            if let Some(control) = state.downcast_ref::<accessibility::Control<Message>>()
                && is_item(control.node.role())
            {
                self.0.push((
                    control.id,
                    control.on_focus.clone(),
                    control.activate.clone(),
                ));
            }
        }
    }

    fn button(
        name: &str,
        role: accessibility::Role,
        input: Input,
        enabled: bool,
    ) -> Button<'_, Message, ()> {
        Button::new(Space::new().width(100).height(24))
            .name(name)
            .role(role)
            .on_press_maybe(enabled.then_some(Message::DesktopMenu(input)))
    }

    #[test]
    fn native_menu_bar_has_real_children_and_follows_keyboard_focus_without_activating() {
        let headers = HeaderIds::default();
        let mut bar = Region {
            content: Row::new()
                .push(
                    button("File", accessibility::Role::MenuItem, Input::Open(0), true)
                        .expanded(false),
                )
                .push(
                    button("View", accessibility::Role::MenuItem, Input::Open(1), true)
                        .expanded(false),
                )
                .into(),
            role: accessibility::Role::MenuBar,
            name: "Application menu".into(),
            focus: Some(1),
            override_focus: true,
            inputs: vec![Input::FocusHeader(0), Input::FocusHeader(1)],
            shortcuts: vec![],
            headers: headers.clone(),
            popup_for: None,
        };
        let mut tree = Tree::new(&bar as &dyn Widget<Message, Theme, ()>);
        let node = bar.layout(
            &mut tree,
            &(),
            &layout::Limits::new(Size::ZERO, Size::new(300.0, 30.0)),
        );
        let (snapshot, bridge, window) =
            accessibility::snapshot_widget(&mut bar, &mut tree, Layout::new(&node), &());
        let nodes: std::collections::HashMap<_, _> = snapshot.nodes.into_iter().collect();
        let header_ids = headers.lock().unwrap().clone();
        let parent = nodes
            .iter()
            .find(|(_, node)| node.role() == accessibility::Role::MenuBar)
            .unwrap();
        assert_eq!(parent.1.children(), header_ids);
        assert_eq!(snapshot.focus, header_ids[1]);
        assert_eq!(
            nodes[&header_ids[1]].has_popup(),
            Some(accesskit::HasPopup::Menu)
        );
        assert_eq!(nodes[&header_ids[1]].is_expanded(), Some(false));
        assert_eq!(nodes[&header_ids[1]].access_key(), Some("v"));
        let root = nodes
            .values()
            .find(|node| node.role() == accessibility::Role::Window)
            .unwrap();
        assert_eq!(root.children(), [*parent.0]);

        let mut routes = Routes::default();
        bar.operate(&mut tree, Layout::new(&node), &(), &mut routes);
        assert!(matches!(
            routes.0[1].1,
            Some(Message::DesktopMenu(Input::FocusHeader(1)))
        ));
        assert!(matches!(
            routes.0[1].2,
            Some(Message::DesktopMenu(Input::Open(1)))
        ));
        use iced::futures::StreamExt;
        use iced_runtime::{Action as RuntimeAction, window::Action as WindowAction};
        let mut actions =
            iced_runtime::task::into_stream(bridge.dispatch(accessibility::Request {
                window,
                replay: None,
                action: accesskit::ActionRequest {
                    action: accesskit::Action::Focus,
                    target: header_ids[1],
                    data: None,
                },
            }))
            .unwrap();
        let Some(RuntimeAction::Window(WindowAction::GetMode(id, reply))) =
            iced::futures::executor::block_on(actions.next())
        else {
            panic!("native menu focus must query its owning window");
        };
        assert_eq!(id, window);
        reply.send(iced::window::Mode::Windowed).unwrap();
        let outputs = iced::futures::executor::block_on(actions.collect::<Vec<_>>());
        assert!(
            matches!(outputs.as_slice(), [
            RuntimeAction::Window(WindowAction::Minimize(id, false)),
            RuntimeAction::Window(WindowAction::GainFocus(focused)),
            RuntimeAction::Output(Message::DesktopMenu(Input::FocusHeader(1)))
        ] if *id == window && *focused == window),
            "native menu focus restores and activates its owner before routing logical focus"
        );
        let mut state = DesktopState::default();
        assert!(
            state
                .input(
                    Input::FocusHeader(1),
                    &[
                        Menu {
                            title: "File",
                            items: vec![]
                        },
                        Menu {
                            title: "View",
                            items: vec![]
                        }
                    ],
                    true
                )
                .is_none()
        );
        assert_eq!(state.focused, Some(1));
        assert!(!state.open, "native focus does not activate the menu");
        assert!(
            state.editor,
            "native menu focus retains document editor availability"
        );
    }

    #[test]
    fn native_popup_reports_check_states_shortcuts_parent_and_disabled_action_refusal() {
        let header_id = accessibility::Control::<Message>::new(accessibility::Role::MenuItem).id;
        let mut popup = Region {
            content: Column::new()
                .push(
                    button(
                        "Show Inspector",
                        accessibility::Role::MenuItemCheckBox,
                        Input::Pick(0),
                        true,
                    )
                    .checked(false),
                )
                .push(
                    button(
                        "Capture",
                        accessibility::Role::MenuItemRadio,
                        Input::Pick(1),
                        true,
                    )
                    .checked(true),
                )
                .push(button(
                    "Unavailable",
                    accessibility::Role::MenuItem,
                    Input::Pick(2),
                    false,
                ))
                .into(),
            role: accessibility::Role::Menu,
            name: "View".into(),
            focus: Some(1),
            override_focus: true,
            inputs: vec![
                Input::FocusItem(0),
                Input::FocusItem(1),
                Input::FocusItem(2),
            ],
            shortcuts: vec![Some("Ctrl+I".into()), None, None],
            headers: Arc::new(Mutex::new(vec![header_id])),
            popup_for: Some(0),
        };
        let mut tree = Tree::new(&popup as &dyn Widget<Message, Theme, ()>);
        let node = popup.layout(
            &mut tree,
            &(),
            &layout::Limits::new(Size::ZERO, Size::new(300.0, 120.0)),
        );
        let (snapshot, bridge, window) =
            accessibility::snapshot_widget(&mut popup, &mut tree, Layout::new(&node), &());
        let nodes: std::collections::HashMap<_, _> = snapshot.nodes.into_iter().collect();
        let parent = nodes
            .values()
            .find(|node| node.role() == accessibility::Role::Menu)
            .unwrap();
        assert_eq!(parent.popup_for(), Some(header_id));
        let ids = parent.children();
        assert_eq!(ids.len(), 3);
        assert_eq!(nodes[&ids[0]].role(), accessibility::Role::MenuItemCheckBox);
        assert_eq!(
            nodes[&ids[0]].toggled(),
            Some(accessibility::Toggled::False)
        );
        assert_eq!(nodes[&ids[0]].keyboard_shortcut(), Some("Ctrl+I"));
        assert_eq!(nodes[&ids[1]].role(), accessibility::Role::MenuItemRadio);
        assert_eq!(nodes[&ids[1]].toggled(), Some(accessibility::Toggled::True));
        assert_eq!(snapshot.focus, ids[1]);
        assert!(nodes[&ids[2]].is_disabled());
        assert_eq!(
            bridge
                .dispatch(accessibility::Request {
                    window,
                    replay: None,
                    action: accesskit::ActionRequest {
                        action: accesskit::Action::Click,
                        target: ids[2],
                        data: None
                    },
                })
                .units(),
            0
        );
    }
}
