//! Actual focusable controls and delivered pointer events determine document
//! emphasis; scope markers never alter the controls' own focus or selection.
use super::*;
use crate::gui::button::Button;
use iced::{
    Theme,
    widget::{Row, Space, Stack, TextInput},
};

type Root = Guard<'static, &'static str, Theme, ()>;
fn wrap(content: Element<'static, &'static str, Theme, ()>, suspended: bool) -> Root {
    Guard {
        content,
        os: Os::CURRENT,
        background: false,
        desktop_active: false,
        edit: None,
        on_editor_focus: Some(|focused| {
            if focused {
                "editor:true"
            } else {
                "editor:false"
            }
        }),
        on_popup: None,
        on_escape: None,
        on_content_focus: Some(|area| match area {
            Area::Image => "area:image",
            Area::CameraList => "area:list",
            Area::Other => "area:other",
        }),
        suspend_content_focus: suspended,
    }
}
fn body() -> Element<'static, &'static str, Theme, ()> {
    Row::new()
        .push(scope(
            Button::<_, ()>::new(Space::new().width(90).height(60))
                .padding(0)
                .id("camera-row")
                .on_press("camera")
                .into(),
            Area::CameraList,
        ))
        .push(scope(
            Button::<_, ()>::new(Space::new().width(90).height(60))
                .padding(0)
                .id("image")
                .on_press("image")
                .into(),
            Area::Image,
        ))
        .push(
            TextInput::new("", "draft")
                .id("editor")
                .on_input(|_| "edit")
                .width(120),
        )
        .into()
}
fn layout(root: &mut Root, tree: &mut Tree) -> layout::Node {
    root.layout(
        tree,
        &(),
        &layout::Limits::new(Size::ZERO, Size::new(400.0, 180.0)),
    )
}
fn event(
    root: &mut Root,
    tree: &mut Tree,
    node: &layout::Node,
    event: Event,
    position: Option<iced::Point>,
    viewport: Rectangle,
) -> Vec<&'static str> {
    event_result(root, tree, node, event, position, viewport).0
}
fn event_result(
    root: &mut Root,
    tree: &mut Tree,
    node: &layout::Node,
    event: Event,
    position: Option<iced::Point>,
    viewport: Rectangle,
) -> (Vec<&'static str>, window::RedrawRequest) {
    let mut messages = Vec::new();
    let mut shell = Shell::new(&mut messages);
    root.update(
        tree,
        &event,
        Layout::new(node),
        position.map_or(mouse::Cursor::Unavailable, mouse::Cursor::Available),
        &(),
        &mut iced::advanced::clipboard::Null,
        &mut shell,
        &viewport,
    );
    let request = shell.redraw_request();
    (messages, request)
}
fn redraw(root: &mut Root, tree: &mut Tree, node: &layout::Node) -> Vec<&'static str> {
    event(
        root,
        tree,
        node,
        Event::Window(window::Event::RedrawRequested(std::time::Instant::now())),
        None,
        Rectangle::with_size(node.size()),
    )
    .into_iter()
    .filter(|message| message.starts_with("area:"))
    .collect()
}
fn apply(root: &mut Root, tree: &mut Tree, node: &layout::Node, action: impl Operation) {
    root.operate(tree, Layout::new(node), &(), &mut { action });
}
fn focus(root: &mut Root, tree: &mut Tree, node: &layout::Node, id: &'static str) {
    apply(
        root,
        tree,
        node,
        operation::focusable::focus::<()>(Id::new(id)),
    );
}
fn press(root: &mut Root, tree: &mut Tree, node: &layout::Node, position: iced::Point) {
    event(
        root,
        tree,
        node,
        Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
        Some(position),
        Rectangle::with_size(node.size()),
    );
}
fn editor(tree: &Tree) -> Option<&iced::widget::text_input::State<()>> {
    if tree.tag == tree::Tag::of::<iced::widget::text_input::State<()>>() {
        Some(tree.state.downcast_ref())
    } else {
        tree.children.iter().find_map(editor)
    }
}

#[test]
fn scoped_keyboard_focus_changes_emphasis_once_without_changing_editor_selection() {
    let mut root = wrap(body(), false);
    let mut tree = Tree::new(&root as &dyn Widget<&str, Theme, ()>);
    let node = layout(&mut root, &mut tree);
    assert_eq!(redraw(&mut root, &mut tree, &node), ["area:image"]);
    assert!(redraw(&mut root, &mut tree, &node).is_empty());
    for (id, expected) in [
        ("camera-row", "area:list"),
        ("image", "area:image"),
        ("editor", "area:other"),
    ] {
        focus(&mut root, &mut tree, &node, id);
        assert_eq!(redraw(&mut root, &mut tree, &node), [expected]);
        assert!(redraw(&mut root, &mut tree, &node).is_empty());
    }
    apply(
        &mut root,
        &mut tree,
        &node,
        operation::text_input::select_all::<()>(Id::new("editor")),
    );
    let before = editor(&tree).unwrap().cursor();
    redraw(&mut root, &mut tree, &node);
    assert!(editor(&tree).unwrap().is_focused());
    assert_eq!(editor(&tree).unwrap().cursor(), before);
    event(
        &mut root,
        &mut tree,
        &node,
        Event::Window(window::Event::Focused),
        None,
        Rectangle::with_size(node.size()),
    );
    assert_eq!(
        redraw(&mut root, &mut tree, &node),
        ["area:other"],
        "native activation republishes unchanged context"
    );
}

#[test]
fn mouse_and_touch_hints_survive_when_no_widget_has_keyboard_focus() {
    let mut root = wrap(body(), false);
    let mut tree = Tree::new(&root as &dyn Widget<&str, Theme, ()>);
    let node = layout(&mut root, &mut tree);
    press(&mut root, &mut tree, &node, iced::Point::new(30.0, 30.0));
    assert_eq!(redraw(&mut root, &mut tree, &node), ["area:list"]);
    assert_eq!(
        tree.state
            .downcast_ref::<State>()
            .content_focus
            .pointer_hint,
        Area::CameraList
    );
    event(
        &mut root,
        &mut tree,
        &node,
        Event::Touch(iced::touch::Event::FingerPressed {
            id: iced::touch::Finger(1),
            position: iced::Point::new(120.0, 30.0),
        }),
        None,
        Rectangle::with_size(node.size()),
    );
    assert_eq!(redraw(&mut root, &mut tree, &node), ["area:image"]);
    press(&mut root, &mut tree, &node, iced::Point::new(250.0, 15.0));
    redraw(&mut root, &mut tree, &node);
    apply(
        &mut root,
        &mut tree,
        &node,
        operation::focusable::unfocus::<()>(),
    );
    assert!(redraw(&mut root, &mut tree, &node).is_empty());
    assert_eq!(
        tree.state
            .downcast_ref::<State>()
            .content_focus
            .pointer_hint,
        Area::Other
    );
}

#[test]
fn covered_or_clipped_content_cannot_claim_an_unscoped_pointer_target() {
    let image = scope(Space::new().width(300).height(120).into(), Area::Image);
    let overlay = Button::<_, ()>::new(Space::new().width(60).height(60))
        .padding(0)
        .on_press("overlay");
    let mut root = wrap(Stack::new().push(image).push(overlay).into(), false);
    let mut tree = Tree::new(&root as &dyn Widget<&str, Theme, ()>);
    let node = layout(&mut root, &mut tree);
    press(&mut root, &mut tree, &node, iced::Point::new(20.0, 20.0));
    assert_eq!(
        redraw(&mut root, &mut tree, &node),
        ["area:other"],
        "unscoped overlay takes the press before the underlying image"
    );
    let (messages, request) = event_result(
        &mut root,
        &mut tree,
        &node,
        Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
        Some(iced::Point::new(100.0, 90.0)),
        Rectangle::with_size(node.size()),
    );
    assert!(messages.is_empty(), "the blank image has no app action");
    assert_eq!(request, window::RedrawRequest::NextFrame);
    assert_eq!(redraw(&mut root, &mut tree, &node), ["area:image"]);
    press(&mut root, &mut tree, &node, iced::Point::new(20.0, 20.0));
    assert_eq!(redraw(&mut root, &mut tree, &node), ["area:other"]);
    event(
        &mut root,
        &mut tree,
        &node,
        Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
        Some(iced::Point::new(100.0, 90.0)),
        Rectangle::with_size(Size::new(50.0, 50.0)),
    );
    assert!(redraw(&mut root, &mut tree, &node).is_empty());
    assert_eq!(
        tree.state
            .downcast_ref::<State>()
            .content_focus
            .pointer_hint,
        Area::Other,
        "content outside its clip does not claim the press"
    );
}

fn modal(open: bool) -> Root {
    let mut parent = wrap(body(), false);
    parent.background = open;
    parent.on_content_focus = None;
    let content: Element<'static, &str, Theme, ()> = if open {
        TextInput::new("", "sheet draft")
            .id("sheet-editor")
            .on_input(|_| "sheet-edit")
            .width(180)
            .into()
    } else {
        Space::new().into()
    };
    wrap(
        Stack::new().push(Element::new(parent)).push(content).into(),
        open,
    )
}
#[test]
fn modal_suspension_preserves_document_hint_and_editor_focus_and_selection() {
    let mut root = modal(false);
    let mut tree = Tree::new(&root as &dyn Widget<&str, Theme, ()>);
    let mut node = layout(&mut root, &mut tree);
    press(&mut root, &mut tree, &node, iced::Point::new(30.0, 30.0));
    assert_eq!(redraw(&mut root, &mut tree, &node), ["area:list"]);
    focus(&mut root, &mut tree, &node, "editor");
    apply(
        &mut root,
        &mut tree,
        &node,
        operation::text_input::select_all::<()>(Id::new("editor")),
    );
    assert_eq!(redraw(&mut root, &mut tree, &node), ["area:other"]);
    let selection = editor(&tree).unwrap().cursor();
    root = modal(true);
    root.diff(&mut tree);
    node = layout(&mut root, &mut tree);
    focus(&mut root, &mut tree, &node, "sheet-editor");
    press(&mut root, &mut tree, &node, iced::Point::new(20.0, 15.0));
    event(
        &mut root,
        &mut tree,
        &node,
        Event::Window(window::Event::Focused),
        None,
        Rectangle::with_size(node.size()),
    );
    assert!(redraw(&mut root, &mut tree, &node).is_empty());
    let tracker = &tree.state.downcast_ref::<State>().content_focus;
    assert_eq!(tracker.pointer_hint, Area::CameraList);
    assert_eq!(tracker.reported, Some(Area::Other));
    root = modal(false);
    root.diff(&mut tree);
    node = layout(&mut root, &mut tree);
    assert!(redraw(&mut root, &mut tree, &node).is_empty());
    assert!(editor(&tree).unwrap().is_focused());
    assert_eq!(editor(&tree).unwrap().cursor(), selection);
    apply(
        &mut root,
        &mut tree,
        &node,
        operation::focusable::unfocus::<()>(),
    );
    assert_eq!(
        redraw(&mut root, &mut tree, &node),
        ["area:list"],
        "closing restores the document's retained pointer hint"
    );
}
