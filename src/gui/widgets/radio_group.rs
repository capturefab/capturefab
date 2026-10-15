//! Segment groups retain ordinary button pointer behavior while arrow keys
//! move keyboard focus and select the next enabled choice together.
use crate::gui::accessibility;
use iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, overlay, renderer,
    widget::{Id, Operation, Tree, operation::Focusable, tree},
};
use iced::{Element, Event, Length, Rectangle, Size, Theme, Vector, keyboard, mouse, window};

pub(super) fn wrap<'a, M: Clone + 'static>(
    content: Element<'a, M>,
    choices: Vec<M>,
    tabs: bool,
    name: &'a str,
) -> Element<'a, M> {
    Element::new(Group {
        content,
        choices,
        tabs,
        name,
    })
}

struct Group<'a, M, R = iced::Renderer> {
    content: Element<'a, M, Theme, R>,
    choices: Vec<M>,
    tabs: bool,
    name: &'a str,
}

struct State<M> {
    window_focused: bool,
    control: accessibility::Control<M>,
}
impl<M> Default for State<M> {
    fn default() -> Self {
        Self {
            window_focused: true,
            control: accessibility::Control::new(accessibility::Role::TabList),
        }
    }
}

#[derive(Default)]
struct Focused {
    ids: Vec<Id>,
    current: Option<usize>,
}
impl Operation for Focused {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        operate(self);
    }
    fn focusable(&mut self, id: Option<&Id>, _: Rectangle, state: &mut dyn Focusable) {
        if let Some(id) = id {
            if state.is_focused() {
                self.current = Some(self.ids.len());
            }
            self.ids.push(id.clone());
        }
    }
}

impl<'a, M: Clone + 'static, R: renderer::Renderer + 'a> Widget<M, Theme, R> for Group<'a, M, R> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State<M>>()
    }
    fn state(&self) -> tree::State {
        tree::State::new(State::<M>::default())
    }
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(self.content.as_widget())]
    }
    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(&[self.content.as_widget()]);
    }
    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }
    fn size_hint(&self) -> Size<Length> {
        self.content.as_widget().size_hint()
    }
    fn layout(&mut self, tree: &mut Tree, renderer: &R, limits: &layout::Limits) -> layout::Node {
        self.content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }
    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &R,
        operation: &mut dyn Operation,
    ) {
        let mut children = ChoiceChildren::<M> {
            ids: Vec::new(),
            role: if self.tabs {
                accessibility::Role::Tab
            } else {
                accessibility::Role::RadioButton
            },
            marker: std::marker::PhantomData,
        };
        self.content.as_widget_mut().operate(
            &mut tree.children[0],
            layout,
            renderer,
            &mut children,
        );
        let control = &mut tree.state.downcast_mut::<State<M>>().control;
        control.node = accessibility::Node::new(if self.tabs {
            accessibility::Role::TabList
        } else {
            accessibility::Role::RadioGroup
        });
        control.node.set_label(self.name);
        control.node.set_children(children.ids);
        control.focusable = false;
        let id = control.focus_id.clone();
        operation.custom(Some(&id), layout.bounds(), control);
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
        let state = tree.state.downcast_mut::<State<M>>();
        match event {
            Event::Window(window::Event::Focused) => state.window_focused = true,
            Event::Window(window::Event::Unfocused) => state.window_focused = false,
            _ => {}
        }
        if let Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. }) = event
            && state.window_focused
            && !shell.is_event_captured()
            && !modifiers.alt()
            && !modifiers.control()
            && !modifiers.logo()
        {
            use keyboard::key::Named;
            let step = match key {
                keyboard::Key::Named(Named::ArrowRight | Named::ArrowDown) => Some(1isize),
                keyboard::Key::Named(Named::ArrowLeft | Named::ArrowUp) => Some(-1),
                keyboard::Key::Named(Named::Home) => Some(isize::MIN),
                keyboard::Key::Named(Named::End) => Some(isize::MAX),
                _ => None,
            };
            if let Some(step) = step {
                let mut focused = Focused::default();
                self.content.as_widget_mut().operate(
                    &mut tree.children[0],
                    layout,
                    renderer,
                    &mut focused,
                );
                if let Some(current) = focused.current {
                    let next = match step {
                        isize::MIN => 0,
                        isize::MAX => focused.ids.len() - 1,
                        step => (current as isize + step).rem_euclid(focused.ids.len() as isize)
                            as usize,
                    };
                    if let Some(message) = self.choices.get(next) {
                        let mut focus = iced::advanced::widget::operation::focusable::focus(
                            focused.ids[next].clone(),
                        );
                        self.content.as_widget_mut().operate(
                            &mut tree.children[0],
                            layout,
                            renderer,
                            &mut focus,
                        );
                        shell.publish(message.clone());
                        shell.capture_event();
                        shell.request_redraw();
                        return;
                    }
                }
            }
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
    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &R,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, M, Theme, R>> {
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}

struct ChoiceChildren<M> {
    ids: Vec<accesskit::NodeId>,
    role: accessibility::Role,
    marker: std::marker::PhantomData<fn() -> M>,
}
impl<M: 'static> Operation for ChoiceChildren<M> {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        operate(self);
    }
    fn custom(&mut self, _: Option<&Id>, _: Rectangle, state: &mut dyn std::any::Any) {
        if let Some(control) = state.downcast_ref::<accessibility::Control<M>>()
            && control.node.role() == self.role
        {
            self.ids.push(control.id);
        }
    }
}

#[cfg(test)]
mod semantics_tests;
#[cfg(test)]
mod tests {
    use super::*;
    use crate::gui::button::Button;

    #[test]
    fn native_tabs_expose_group_relationship_and_current_selection() {
        #[derive(Default)]
        struct Semantics(Vec<(accesskit::NodeId, accessibility::Node)>);
        impl Operation for Semantics {
            fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
                operate(self);
            }
            fn custom(&mut self, _: Option<&Id>, _: Rectangle, state: &mut dyn std::any::Any) {
                if let Some(control) = state.downcast_ref::<accessibility::Control<&str>>() {
                    self.0.push((control.id, control.node.clone()));
                }
            }
        }
        let content = iced::widget::Row::new()
            .push(
                Button::<_, ()>::new(iced::widget::Space::new())
                    .name("Features")
                    .role(accessibility::Role::Tab)
                    .selected(false)
                    .id("inspector-tab-0")
                    .on_press("features"),
            )
            .push(
                Button::<_, ()>::new(iced::widget::Space::new())
                    .name("Capture")
                    .role(accessibility::Role::Tab)
                    .selected(true)
                    .id("inspector-tab-1")
                    .on_press("capture"),
            );
        let mut group = Group {
            content: Element::new(content),
            choices: vec!["features", "capture"],
            tabs: true,
            name: "Inspector panels",
        };
        let mut tree = Tree::new(&group as &dyn Widget<&str, Theme, ()>);
        let node = group.layout(
            &mut tree,
            &(),
            &layout::Limits::new(Size::ZERO, Size::new(300.0, 30.0)),
        );
        let mut semantics = Semantics::default();
        group.operate(&mut tree, Layout::new(&node), &(), &mut semantics);
        let [(parent_id, parent), (first_id, first), (second_id, second)] = &semantics.0[..] else {
            panic!("missing tab semantics")
        };
        assert_eq!(parent.role(), accessibility::Role::TabList);
        assert_eq!(parent.children(), [*first_id, *second_id]);
        assert_ne!(parent_id, first_id);
        assert_eq!(first.role(), accessibility::Role::Tab);
        assert_eq!(first.is_selected(), Some(false));
        assert_eq!(second.is_selected(), Some(true));
        assert_eq!(
            first.toggled(),
            None,
            "tabs are selected rather than radio toggles"
        );
        apply_focus(&mut group, &mut tree, &node, "inspector-tab-1");
        let mut focused = Focused::default();
        group.operate(&mut tree, Layout::new(&node), &(), &mut focused);
        assert_eq!(focused.current, Some(1));
    }

    fn apply_focus(
        group: &mut Group<'_, &'static str, ()>,
        tree: &mut Tree,
        node: &layout::Node,
        id: &'static str,
    ) {
        group.operate(
            tree,
            Layout::new(node),
            &(),
            &mut iced::advanced::widget::operation::focusable::focus(Id::new(id)),
        );
    }

    #[test]
    fn segment_arrows_move_focus_select_wrap_and_skip_disabled_choices() {
        for disabled in [false, true] {
            let content = iced::widget::Row::new()
                .push(
                    Button::<_, ()>::new(iced::widget::Space::new())
                        .id("first")
                        .on_press("first"),
                )
                .push(
                    Button::<_, ()>::new(iced::widget::Space::new())
                        .id("second")
                        .on_press_maybe((!disabled).then_some("second")),
                )
                .push(
                    Button::<_, ()>::new(iced::widget::Space::new())
                        .id("third")
                        .on_press("third"),
                );
            let mut group = Group {
                content: Element::new(content),
                tabs: false,
                name: "Test choices",
                choices: if disabled {
                    vec!["first", "third"]
                } else {
                    vec!["first", "second", "third"]
                },
            };
            let mut tree = Tree::new(&group as &dyn Widget<&str, Theme, ()>);
            let node = group.layout(
                &mut tree,
                &(),
                &layout::Limits::new(Size::ZERO, Size::new(300.0, 30.0)),
            );
            group.operate(
                &mut tree,
                Layout::new(&node),
                &(),
                &mut iced::advanced::widget::operation::focusable::focus(Id::new("first")),
            );
            let mut messages = Vec::new();
            for key in [
                keyboard::key::Named::ArrowRight,
                keyboard::key::Named::End,
                keyboard::key::Named::ArrowRight,
                keyboard::key::Named::Home,
            ] {
                let event = Event::Keyboard(keyboard::Event::KeyPressed {
                    key: keyboard::Key::Named(key),
                    modified_key: keyboard::Key::Named(key),
                    physical_key: keyboard::key::Physical::Unidentified(
                        keyboard::key::NativeCode::Unidentified,
                    ),
                    location: keyboard::Location::Standard,
                    modifiers: keyboard::Modifiers::empty(),
                    text: None,
                    repeat: false,
                });
                let mut shell = Shell::new(&mut messages);
                group.update(
                    &mut tree,
                    &event,
                    Layout::new(&node),
                    mouse::Cursor::Unavailable,
                    &(),
                    &mut iced::advanced::clipboard::Null,
                    &mut shell,
                    &Rectangle::with_size(node.size()),
                );
                assert!(shell.is_event_captured());
            }
            assert_eq!(
                messages,
                [
                    if disabled { "third" } else { "second" },
                    "third",
                    "first",
                    "first"
                ]
            );
            let mut focused = Focused::default();
            group.operate(&mut tree, Layout::new(&node), &(), &mut focused);
            assert_eq!(focused.current, Some(0));
            assert_eq!(focused.ids.len(), if disabled { 2 } else { 3 });
        }
    }
}
