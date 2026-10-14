//! Exercise focus through actual control/container trees, including the scope
//! operations used by the root update and the preserved modal background.
use super::*;
use crate::gui::button::Button;
use iced::{
    Theme,
    widget::{Column, Container, Row, Space, Stack, TextInput},
};

type TestGuard = Guard<'static, &'static str, Theme, ()>;

fn wrap(content: Element<'static, &'static str, Theme, ()>, background: bool) -> TestGuard {
    Guard {
        content,
        os: Os::CURRENT,
        background,
        desktop_active: false,
        edit: None,
        on_editor_focus: None,
        on_popup: None,
        on_escape: None,
        on_content_focus: None,
        suspend_content_focus: false,
    }
}

fn control(id: &'static str, message: Option<&'static str>) -> Button<'static, &'static str, ()> {
    Button::new(Space::new().width(40).height(30))
        .id(id)
        .on_press_maybe(message)
}

fn layout(root: &mut TestGuard, tree: &mut Tree) -> layout::Node {
    root.layout(
        tree,
        &(),
        &layout::Limits::new(Size::ZERO, Size::new(400.0, 300.0)),
    )
}

fn apply(
    root: &mut TestGuard,
    tree: &mut Tree,
    node: &layout::Node,
    action: impl Operation + 'static,
) {
    let mut action: Box<dyn Operation> = Box::new(action);
    loop {
        root.operate(tree, Layout::new(node), &(), action.as_mut());
        match action.finish() {
            operation::Outcome::Chain(next) => action = next,
            _ => break,
        }
    }
}

fn focused(root: &mut TestGuard, tree: &mut Tree, node: &layout::Node) -> Option<Id> {
    let mut find = operation::focusable::find_focused();
    root.operate(
        tree,
        Layout::new(node),
        &(),
        &mut operation::black_box(&mut find),
    );
    match find.finish() {
        operation::Outcome::Some(id) => Some(id),
        _ => None,
    }
}

fn count(
    root: &mut TestGuard,
    tree: &mut Tree,
    node: &layout::Node,
    scope: Option<&'static str>,
) -> operation::focusable::Count {
    let mut count: Box<dyn Operation<operation::focusable::Count>> = match scope {
        Some(scope) => Box::new(operation::scope(
            Id::new(scope),
            operation::focusable::count(),
        )),
        None => Box::new(operation::focusable::count()),
    };
    root.operate(
        tree,
        Layout::new(node),
        &(),
        &mut operation::black_box(count.as_mut()),
    );
    match count.finish() {
        operation::Outcome::Some(count) => count,
        _ => panic!("missing count"),
    }
}

fn step(root: &mut TestGuard, tree: &mut Tree, node: &layout::Node, back: bool) {
    for _ in 0..2 {
        let action: Box<dyn Operation> = if back {
            Box::new(operation::focusable::focus_previous::<()>())
        } else {
            Box::new(operation::focusable::focus_next::<()>())
        };
        apply(root, tree, node, operation::scope(Id::new("sheet"), action));
        let count = count(root, tree, node, Some("sheet"));
        if count.focused.is_some() || count.total == 0 {
            break;
        }
    }
}

fn event(
    root: &mut TestGuard,
    tree: &mut Tree,
    node: &layout::Node,
    event: Event,
) -> Vec<&'static str> {
    let mut messages = Vec::new();
    root.update(
        tree,
        &event,
        Layout::new(node),
        mouse::Cursor::Unavailable,
        &(),
        &mut iced::advanced::clipboard::Null,
        &mut Shell::new(&mut messages),
        &Rectangle::with_size(Size::new(400.0, 300.0)),
    );
    messages
}

fn redraw(root: &mut TestGuard, tree: &mut Tree, node: &layout::Node) {
    event(
        root,
        tree,
        node,
        Event::Window(window::Event::RedrawRequested(std::time::Instant::now())),
    );
}

fn stream(enabled: bool) -> TestGuard {
    wrap(
        Row::new()
            .push(control("start", enabled.then_some("toggle")))
            .push(control("other", Some("other")))
            .into(),
        false,
    )
}

#[test]
fn command_focus_recovers_after_pending_without_adding_disabled_tab_stops() {
    let mut root = stream(true);
    let mut tree = Tree::new(&root as &dyn Widget<&str, Theme, ()>);
    let mut node = layout(&mut root, &mut tree);
    apply(
        &mut root,
        &mut tree,
        &node,
        operation::focusable::focus::<()>(Id::new("start")),
    );
    root = stream(false);
    root.diff(&mut tree);
    node = layout(&mut root, &mut tree);
    redraw(&mut root, &mut tree, &node);
    assert_eq!(count(&mut root, &mut tree, &node, None).total, 1);
    assert_eq!(focused(&mut root, &mut tree, &node), None);
    root = stream(true);
    root.diff(&mut tree);
    node = layout(&mut root, &mut tree);
    redraw(&mut root, &mut tree, &node);
    assert_eq!(focused(&mut root, &mut tree, &node), Some(Id::new("start")));
}

#[test]
fn command_focus_recovery_never_steals_a_later_native_keyboard_or_pointer_focus() {
    for interaction in ["native", "keyboard", "pointer"] {
        let mut root = stream(true);
        let mut tree = Tree::new(&root as &dyn Widget<&str, Theme, ()>);
        let mut node = layout(&mut root, &mut tree);
        apply(
            &mut root,
            &mut tree,
            &node,
            operation::focusable::focus::<()>(Id::new("start")),
        );
        root = stream(false);
        root.diff(&mut tree);
        node = layout(&mut root, &mut tree);
        match interaction {
            "native" => apply(
                &mut root,
                &mut tree,
                &node,
                operation::focusable::focus::<()>(Id::new("other")),
            ),
            "keyboard" => {
                event(
                    &mut root,
                    &mut tree,
                    &node,
                    super::tests::named_press(
                        keyboard::key::Named::Tab,
                        keyboard::Modifiers::empty(),
                    ),
                );
            }
            "pointer" => {
                event(
                    &mut root,
                    &mut tree,
                    &node,
                    Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
                );
            }
            _ => unreachable!(),
        }
        redraw(&mut root, &mut tree, &node);
        root = stream(true);
        root.diff(&mut tree);
        node = layout(&mut root, &mut tree);
        redraw(&mut root, &mut tree, &node);
        let expected = (interaction == "native").then(|| Id::new("other"));
        assert_eq!(
            focused(&mut root, &mut tree, &node),
            expected,
            "{interaction}"
        );
    }
}

fn editable_form(enabled: bool) -> TestGuard {
    wrap(
        Row::new()
            .push(
                crate::gui::text_input::TextInput::<_, ()>::new("", "unfinished draft")
                    .id("draft")
                    .on_input_maybe(enabled.then_some(|_| "edited")),
            )
            .push(control("other", Some("other")))
            .into(),
        false,
    )
}

#[test]
fn editor_focus_recovers_after_a_frozen_save_without_stealing_newer_input() {
    for interaction in ["none", "native", "keyboard", "pointer"] {
        let mut root = editable_form(true);
        let mut tree = Tree::new(&root as &dyn Widget<&str, Theme, ()>);
        let mut node = layout(&mut root, &mut tree);
        apply(
            &mut root,
            &mut tree,
            &node,
            operation::focusable::focus::<()>(Id::new("draft")),
        );
        root = editable_form(false);
        root.diff(&mut tree);
        node = layout(&mut root, &mut tree);
        redraw(&mut root, &mut tree, &node);
        assert_eq!(count(&mut root, &mut tree, &node, None).total, 1);
        assert_eq!(focused(&mut root, &mut tree, &node), None);
        match interaction {
            "none" => {}
            "native" => apply(
                &mut root,
                &mut tree,
                &node,
                operation::focusable::focus::<()>(Id::new("other")),
            ),
            "keyboard" => {
                event(
                    &mut root,
                    &mut tree,
                    &node,
                    super::tests::named_press(
                        keyboard::key::Named::Tab,
                        keyboard::Modifiers::empty(),
                    ),
                );
            }
            "pointer" => {
                event(
                    &mut root,
                    &mut tree,
                    &node,
                    Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
                );
            }
            _ => unreachable!(),
        }
        redraw(&mut root, &mut tree, &node);
        root = editable_form(true);
        root.diff(&mut tree);
        node = layout(&mut root, &mut tree);
        redraw(&mut root, &mut tree, &node);
        let expected = match interaction {
            "none" => Some(Id::new("draft")),
            "native" => Some(Id::new("other")),
            _ => None,
        };
        assert_eq!(
            focused(&mut root, &mut tree, &node),
            expected,
            "{interaction}"
        );
    }
}

fn sheet(open: bool) -> TestGuard {
    let body = wrap(
        Row::new()
            .push(control("trigger", Some("open")))
            .push(control("background", Some("wrong")))
            .width(400)
            .height(300)
            .into(),
        open,
    );
    let mut layers = Stack::new().push(Element::new(body));
    if open {
        layers = layers.push(
            Container::new(
                Column::new()
                    .push(control("sheet-close", Some("close")))
                    .push(control("disabled", None))
                    .push(
                        TextInput::new("", "value")
                            .id("sheet-editor")
                            .on_input(|_| "typed"),
                    )
                    .push(control("sheet-last", Some("last"))),
            )
            .id("sheet"),
        );
    }
    wrap(Container::new(layers).id("workbench-root").into(), false)
}

#[test]
fn sheet_tabs_wrap_skip_disabled_controls_and_close_returns_to_keyboard_trigger() {
    let mut root = sheet(false);
    let mut tree = Tree::new(&root as &dyn Widget<&str, Theme, ()>);
    let mut node = layout(&mut root, &mut tree);
    apply(
        &mut root,
        &mut tree,
        &node,
        operation::focusable::focus::<()>(Id::new("trigger")),
    );
    assert_eq!(
        event(
            &mut root,
            &mut tree,
            &node,
            super::tests::named_press(keyboard::key::Named::Enter, keyboard::Modifiers::empty())
        ),
        ["open"]
    );
    root = sheet(true);
    root.diff(&mut tree);
    node = layout(&mut root, &mut tree);
    apply(
        &mut root,
        &mut tree,
        &node,
        operation::focusable::focus::<()>(Id::new("sheet-close")),
    );
    assert_eq!(count(&mut root, &mut tree, &node, Some("sheet")).total, 3);
    for (back, target) in [
        (false, "sheet-editor"),
        (false, "sheet-last"),
        (false, "sheet-close"),
        (true, "sheet-last"),
        (true, "sheet-editor"),
        (true, "sheet-close"),
    ] {
        step(&mut root, &mut tree, &node, back);
        assert_eq!(focused(&mut root, &mut tree, &node), Some(Id::new(target)));
    }
    assert_eq!(
        event(
            &mut root,
            &mut tree,
            &node,
            super::tests::named_press(keyboard::key::Named::Enter, keyboard::Modifiers::empty())
        ),
        ["close"]
    );
    root = sheet(false);
    root.diff(&mut tree);
    node = layout(&mut root, &mut tree);
    assert_eq!(
        focused(&mut root, &mut tree, &node),
        Some(Id::new("trigger"))
    );
    assert_eq!(
        event(
            &mut root,
            &mut tree,
            &node,
            super::tests::named_press(keyboard::key::Named::Enter, keyboard::Modifiers::empty())
        ),
        ["open"]
    );
}
