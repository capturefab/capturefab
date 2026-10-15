use super::*;
use crate::gui::{Message, accessibility, text_input::TextInput};
use iced::advanced::{Layout, Widget, layout, text, widget::Tree};
use iced::{Alignment, Font, Pixels, Size, Theme};

type Paragraph = iced::advanced::graphics::text::Paragraph;

fn paragraph(value: &str) -> Paragraph {
    <Paragraph as text::Paragraph>::with_text(text::Text {
        content: value,
        bounds: Size::new(f32::INFINITY, 24.0),
        size: Pixels(16.0),
        line_height: text::LineHeight::Relative(1.2),
        font: Font::DEFAULT,
        align_x: text::Alignment::Default,
        align_y: iced::alignment::Vertical::Center,
        shaping: text::Shaping::Advanced,
        wrapping: text::Wrapping::None,
    })
}

fn consumer(published: Published) -> (accesskit_consumer::Tree, NodeId) {
    let id = accessibility::new_node_id();
    let mut input = Node::new(Role::TextInput);
    input.set_children(
        published
            .children
            .iter()
            .map(|(id, _)| *id)
            .collect::<Vec<_>>(),
    );
    if let Some(selection) = published.selection {
        input.set_text_selection(selection);
    }
    let mut nodes = published.children;
    nodes.push((id, input));
    (
        accesskit_consumer::Tree::new(
            accesskit::TreeUpdate {
                nodes,
                tree: Some(accesskit::Tree::new(id)),
                focus: id,
            },
            true,
        ),
        id,
    )
}

#[test]
fn real_bidi_cells_supply_consumer_ranges_hit_tests_and_logical_selection() {
    use crate::gui::text_input::selection::ParagraphGeometry;
    let _renderer = crate::gui::destinations::layout_tests::renderer();
    for source in [
        "Latin text",
        "אבגדה",
        "A🇨🇦e\u{301} אבג 👩‍🚀 آخر Z",
        "אבג abc 123 آخر 🏳️‍🌈",
    ] {
        let paragraph = paragraph(source);
        let cells = paragraph.visual_cells().unwrap();
        let value = Value::new(source);
        assert_eq!(
            cells.len(),
            value.len(),
            "all real shaping cells for {source:?}"
        );
        let mut state = State::default();
        let fallback = accessibility::new_node_id();
        let boundaries: Vec<_> = source
            .grapheme_indices(true)
            .map(|(byte, _)| byte)
            .chain(std::iter::once(source.len()))
            .collect();
        let mut ids = None;
        for origin_x in [9.0, -45.0] {
            let origin = Point::new(origin_x, 12.0);
            let viewport = Rectangle {
                x: 9.0,
                y: 10.0,
                width: 145.0,
                height: 26.0,
            };
            let published = state.update(
                &paragraph,
                source,
                &value,
                cursor::State::Selection {
                    start: value.len() - 1,
                    end: 1,
                },
                origin,
                viewport,
                fallback,
            );
            let current_ids = published
                .children
                .iter()
                .map(|(id, _)| *id)
                .collect::<Vec<_>>();
            if let Some(ids) = &ids {
                assert_eq!(&current_ids, ids);
            }
            ids = Some(current_ids);
            assert!(published.children.iter().all(|(_, node)| {
                node.character_positions().unwrap().len() == node.character_lengths().len()
                    && node.character_widths().unwrap().len() == node.character_lengths().len()
            }));
            assert_eq!(
                state.selection_indices(&published.selection.unwrap()),
                Some((value.len() - 1, 1))
            );
            let (consumer, id) = consumer(published);
            let input = consumer.state().node_by_id(id).unwrap();
            assert_eq!(input.document_range().text(), source);
            assert_eq!(
                input.text_selection().unwrap().text(),
                value.select(1, value.len() - 1).to_string()
            );
            assert_eq!(
                input
                    .text_selection_anchor()
                    .unwrap()
                    .to_global_utf16_index(),
                source[..boundaries[value.len() - 1]].encode_utf16().count()
            );
            assert_eq!(
                input
                    .text_selection_focus()
                    .unwrap()
                    .to_global_utf16_index(),
                source[..boundaries[1]].encode_utf16().count()
            );
            for cell in &cells {
                let expected = clip(
                    Rectangle {
                        x: origin.x + cell.x,
                        y: origin.y,
                        width: cell.width,
                        height: <Paragraph as text::Paragraph>::min_height(&paragraph),
                    },
                    viewport,
                );
                let start = source[..cell.byte_start].encode_utf16().count();
                let end = source[..cell.byte_end].encode_utf16().count();
                let mut range = input.document_range();
                range.set_start(input.text_position_from_global_utf16_index(start).unwrap());
                range.set_end(input.text_position_from_global_utf16_index(end).unwrap());
                assert_eq!(range.text(), &source[cell.byte_start..cell.byte_end]);
                let boxes = range.bounding_boxes();
                assert!(
                    !boxes.is_empty(),
                    "real native range for {source:?}, grapheme {}",
                    cell.grapheme
                );
                let positive: Vec<_> = boxes.iter().filter(|rect| rect.width() > 0.01).collect();
                if expected.width > 0.01 {
                    assert_eq!(positive.len(), 1);
                    let bounds = positive[0];
                    assert!((bounds.x0 - expected.x as f64).abs() < 0.01);
                    assert!((bounds.width() - expected.width as f64).abs() < 0.01);
                    let point = accesskit::Point::new(
                        bounds.x0 + bounds.width() * 0.5,
                        bounds.y0 + bounds.height() * 0.5,
                    );
                    assert_eq!(
                        input.text_position_at_point(point).to_global_utf16_index(),
                        start,
                        "native hit uses the real visual cell for {source:?}, grapheme {}",
                        cell.grapheme
                    );
                } else {
                    assert!(
                        positive.is_empty(),
                        "offscreen text has no visible native character extent"
                    );
                }
            }
            let line = input.line_range_from_index(0).unwrap();
            assert_eq!(
                line.text(),
                source,
                "directional runs still form one editor line"
            );
            assert!(input.line_range_from_index(1).is_none());
        }
    }
}

#[test]
fn fallback_never_invents_unit_or_unsynchronized_paragraph_geometry() {
    let source = "a🇨🇦e\u{301}z";
    let value = Value::new(source);
    let id = accessibility::new_node_id();
    let mut state = State::default();
    let published = state.update(
        &(),
        "",
        &value,
        cursor::State::Selection { start: 3, end: 1 },
        Point::ORIGIN,
        Rectangle::with_size(Size::new(160.0, 24.0)),
        id,
    );
    assert_eq!(published.children.len(), 1);
    assert_eq!(published.children[0].0, id);
    assert_eq!(published.children[0].1.character_lengths(), &[1, 8, 3, 1]);
    assert!(published.children[0].1.character_positions().is_none());
    assert_eq!(
        state.selection_indices(&published.selection.unwrap()),
        Some((3, 1))
    );
    let stale = paragraph("old draft");
    let published = state.update(
        &stale,
        "old draft",
        &value,
        cursor::State::Index(2),
        Point::ORIGIN,
        Rectangle::with_size(Size::new(160.0, 24.0)),
        id,
    );
    assert_eq!(published.children[0].1.value(), Some(source));
    assert!(published.children[0].1.character_positions().is_none());
    assert_eq!(
        state.selection_indices(&published.selection.unwrap()),
        Some((2, 2))
    );
}

#[test]
fn actual_scrolled_mixed_editor_publishes_visible_native_geometry_and_secure_cells() {
    use iced::advanced::widget::{Id, operation};
    use iced::widget::{column, scrollable, space};
    let renderer = crate::gui::destinations::layout_tests::renderer();
    let source = "A long prefix before the mixed text אבג 🇨🇦 آخر e\u{301} 👩‍🚀 tail";
    for secure in [false, true] {
        for alignment in [Alignment::Start, Alignment::Center, Alignment::End] {
            let mut view: iced::Element<'_, Message> = scrollable(column![
                space().height(100),
                TextInput::new("Value", source)
                    .name("Mixed field")
                    .id("native-mixed")
                    .secure(secure)
                    .align_x(alignment)
                    .width(180)
                    .on_input(Message::Address),
                space().height(100),
            ])
            .id("native-scroll")
            .width(210)
            .height(70)
            .into();
            let mut tree = Tree::new(view.as_widget());
            let node = view.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, Size::new(210.0, 70.0)),
            );
            for mut operation in [
                Box::new(operation::focusable::focus(Id::new("native-mixed")))
                    as Box<dyn iced::advanced::widget::Operation<Message>>,
                Box::new(operation::text_input::move_cursor_to_end(Id::new(
                    "native-mixed",
                ))),
                Box::new(operation::scrollable::scroll_to(
                    Id::new("native-scroll"),
                    operation::scrollable::AbsoluteOffset {
                        x: None,
                        y: Some(75.0),
                    },
                )),
            ] {
                let mut erased = operation::black_box(operation.as_mut());
                view.as_widget_mut()
                    .operate(&mut tree, Layout::new(&node), &renderer, &mut erased);
            }
            let (snapshot, _, _) = accessibility::snapshot_widget(
                view.as_widget_mut(),
                &mut tree,
                Layout::new(&node),
                &renderer,
            );
            let (id, field) = snapshot
                .nodes
                .iter()
                .find(|(_, node)| node.label() == Some("Mixed field"))
                .unwrap();
            let bounds = field.bounds().unwrap();
            assert!(
                bounds.y0 >= 24.0 && bounds.y0 < 40.0,
                "ancestor scrolling is applied to the real field"
            );
            let children: Vec<_> = field
                .children()
                .iter()
                .map(|id| {
                    snapshot
                        .nodes
                        .iter()
                        .find(|(child, _)| child == id)
                        .unwrap()
                        .1
                        .clone()
                })
                .collect();
            assert!(
                children
                    .iter()
                    .all(|child| child.character_positions().is_some()),
                "actual renderer must not silently use unit fallback"
            );
            assert_eq!(
                children
                    .iter()
                    .map(|child| child.value().unwrap())
                    .collect::<String>(),
                if secure {
                    Value::new(source).secure().to_string()
                } else {
                    source.into()
                }
            );
            if secure {
                assert!(
                    children
                        .iter()
                        .all(|child| child.text_direction() == Some(TextDirection::LeftToRight))
                );
            } else {
                assert!(
                    children
                        .iter()
                        .any(|child| child.text_direction() == Some(TextDirection::RightToLeft))
                );
            }
            let consumer = accesskit_consumer::Tree::new(snapshot.clone(), true);
            let input = consumer.state().node_by_id(*id).unwrap();
            assert!(!input.document_range().bounding_boxes().is_empty());
            for child in &children {
                let rect = child.bounds().unwrap();
                assert!(rect.x0 >= bounds.x0 - 0.01 && rect.x1 <= bounds.x1 + 0.01);
                assert!(rect.y0 >= bounds.y0 - 0.01 && rect.y1 <= bounds.y1 + 0.01);
            }
            let caret = input.text_selection().unwrap();
            assert!(caret.is_degenerate());
            let boxes = caret.bounding_boxes();
            assert!(!boxes.is_empty());
            assert!(
                boxes
                    .iter()
                    .all(|rect| rect.x0 >= bounds.x0 - 0.01 && rect.x1 <= bounds.x1 + 0.01)
            );
        }
    }
}

#[test]
fn actual_native_cross_run_selection_then_replacement_uses_logical_graphemes() {
    use iced::futures::StreamExt;
    let renderer = crate::gui::destinations::layout_tests::renderer();
    let source = "A🇨🇦e\u{301} אבג 👩‍🚀 آخر Z";
    let mut app = crate::gui::Workbench::new(
        crate::session::SessionHandle::new(),
        "native-bidi-edit".into(),
        true,
        None,
    );
    app.address = source.into();
    let mut input = TextInput::new("Value", source)
        .name("Mixed field")
        .on_input(Message::Address);
    let mut tree = Tree::new(&input as &dyn Widget<Message, Theme, iced::Renderer>);
    let node = input.layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, Size::new(480.0, 40.0)),
    );
    let (snapshot, bridge, window) =
        accessibility::snapshot_widget(&mut input, &mut tree, Layout::new(&node), &renderer);
    let id = snapshot
        .nodes
        .iter()
        .find(|(_, node)| node.label() == Some("Mixed field"))
        .map(|(id, _)| *id)
        .unwrap();
    let consumer = accesskit_consumer::Tree::new(snapshot, true);
    let field = consumer.state().node_by_id(id).unwrap();
    let value = Value::new(source);
    let start = 2;
    let end = value.len() - 2;
    let at = |index| {
        field
            .text_position_from_global_utf16_index(
                value.until(index).to_string().encode_utf16().count(),
            )
            .unwrap()
            .to_raw()
    };
    let selection = TextSelection {
        anchor: at(end),
        focus: at(start),
    };
    assert_ne!(
        selection.anchor.node, selection.focus.node,
        "this selection crosses directional text runs"
    );
    let task = bridge.dispatch(accessibility::Request {
        window,
        replay: None,
        action: accesskit::ActionRequest {
            action: accesskit::Action::SetTextSelection,
            target: id,
            data: Some(accesskit::ActionData::SetTextSelection(selection)),
        },
    });
    let mut actions = iced_runtime::task::into_stream(task).unwrap();
    while let Some(action) = iced::futures::executor::block_on(actions.next()) {
        if let iced_runtime::Action::Widget(mut operation) = action {
            loop {
                input.operate(&mut tree, Layout::new(&node), &renderer, operation.as_mut());
                match operation.finish() {
                    iced::advanced::widget::operation::Outcome::Chain(next) => operation = next,
                    _ => break,
                }
            }
        }
    }
    let (selected, bridge, window) =
        accessibility::snapshot_widget(&mut input, &mut tree, Layout::new(&node), &renderer);
    let consumer = accesskit_consumer::Tree::new(selected, true);
    let field = consumer.state().node_by_id(id).unwrap();
    assert_eq!(
        field.text_selection().unwrap().text(),
        value.select(start, end).to_string()
    );
    assert_eq!(
        field
            .text_selection_anchor()
            .unwrap()
            .to_global_utf16_index(),
        value.until(end).to_string().encode_utf16().count()
    );
    let mut actions = iced_runtime::task::into_stream(bridge.dispatch(accessibility::Request {
        window,
        replay: None,
        action: accesskit::ActionRequest {
            action: accesskit::Action::ReplaceSelectedText,
            target: id,
            data: Some(accesskit::ActionData::Value("👩‍🚀x".into())),
        },
    }))
    .unwrap();
    let Some(iced_runtime::Action::Output(message)) =
        iced::futures::executor::block_on(actions.next())
    else {
        panic!("normal native input callback");
    };
    drop(app.update(message));
    assert_eq!(
        app.address,
        format!(
            "{}👩‍🚀x{}",
            value.until(start),
            value.select(end, value.len())
        )
    );
    input = TextInput::new("Value", &app.address)
        .name("Mixed field")
        .on_input(Message::Address);
    input.diff(&mut tree);
    let node = input.layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, Size::new(480.0, 40.0)),
    );
    let (updated, _, _) =
        accessibility::snapshot_widget(&mut input, &mut tree, Layout::new(&node), &renderer);
    let consumer = accesskit_consumer::Tree::new(updated, true);
    let field = consumer.state().node_by_id(id).unwrap();
    assert_eq!(field.document_range().text(), app.address);
    assert!(field.text_selection().unwrap().is_degenerate());
    let expected = Value::new(&format!("{}👩‍🚀x", value.until(start))).len();
    assert_eq!(
        field
            .text_selection_focus()
            .unwrap()
            .to_global_utf16_index(),
        Value::new(&app.address)
            .until(expected)
            .to_string()
            .encode_utf16()
            .count()
    );
}

#[test]
fn native_text_focus_and_selection_never_submit_but_keyboard_return_does_once() {
    use iced::futures::StreamExt;
    use iced::{Event, keyboard, mouse};
    let renderer = crate::gui::destinations::layout_tests::renderer();
    let source = "prefix e\u{301} אב tail";
    let target = "e\u{301} אב";
    let mut input = TextInput::new("Camera address", source)
        .name("Camera address")
        .on_input(Message::Address)
        .on_submit(Message::ConnectAddress);
    let mut tree = Tree::new(&input as &dyn Widget<Message, Theme, iced::Renderer>);
    let node = input.layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, Size::new(480.0, 40.0)),
    );
    let (snapshot, bridge, window) =
        accessibility::snapshot_widget(&mut input, &mut tree, Layout::new(&node), &renderer);
    let (id, field) = snapshot
        .nodes
        .iter()
        .find(|(_, node)| node.label() == Some("Camera address"))
        .map(|(id, node)| (*id, node))
        .unwrap();
    assert!(
        !field.supports_action(accesskit::Action::Click),
        "AXPress must not be advertised for a text field with a Return callback"
    );
    assert!(field.supports_action(accesskit::Action::Focus));
    assert!(field.supports_action(accesskit::Action::SetTextSelection));
    let request = |action, data| accessibility::Request {
        window,
        replay: None,
        action: accesskit::ActionRequest {
            action,
            target: id,
            data,
        },
    };
    assert_eq!(
        bridge
            .dispatch(request(accesskit::Action::Click, None))
            .units(),
        0,
        "even an unsolicited native Click cannot invoke Connect"
    );
    let mut focus =
        iced_runtime::task::into_stream(bridge.dispatch(request(accesskit::Action::Focus, None)))
            .unwrap();
    let Some(iced_runtime::Action::Window(iced_runtime::window::Action::GetMode(
        queried_window,
        reply,
    ))) = iced::futures::executor::block_on(focus.next())
    else {
        panic!("native Focus checks its owner's visibility before activation");
    };
    assert_eq!(queried_window, window);
    reply.send(iced::window::Mode::Windowed).unwrap();
    assert!(matches!(iced::futures::executor::block_on(focus.next()),
        Some(iced_runtime::Action::Window(iced_runtime::window::Action::Minimize(id, false))) if id == window));
    assert!(matches!(iced::futures::executor::block_on(focus.next()),
        Some(iced_runtime::Action::Window(iced_runtime::window::Action::GainFocus(id))) if id == window));
    let mut operations = 0;
    while let Some(action) = iced::futures::executor::block_on(focus.next()) {
        match action {
            iced_runtime::Action::Widget(mut operation) => {
                operations += 1;
                loop {
                    input.operate(&mut tree, Layout::new(&node), &renderer, operation.as_mut());
                    match operation.finish() {
                        iced::advanced::widget::operation::Outcome::Chain(next) => operation = next,
                        _ => break,
                    }
                }
            }
            iced_runtime::Action::Output(Message::ConnectAddress) => {
                panic!("native focus submitted Connect")
            }
            _ => {}
        }
    }
    assert!(operations > 0);
    assert!(
        tree.children[0]
            .state
            .downcast_ref::<iced::widget::text_input::State<Paragraph>>()
            .is_focused()
    );
    let consumer = accesskit_consumer::Tree::new(snapshot, true);
    let field = consumer.state().node_by_id(id).unwrap();
    assert!(
        !field.is_clickable(&|_| accesskit_consumer::FilterResult::Include),
        "the pinned macOS adapter's AXPress predicate is false"
    );
    let byte_start = source.find(target).unwrap();
    let utf16_start = source[..byte_start].encode_utf16().count();
    let selection = TextSelection {
        anchor: field
            .text_position_from_global_utf16_index(utf16_start)
            .unwrap()
            .to_raw(),
        focus: field
            .text_position_from_global_utf16_index(utf16_start + target.encode_utf16().count())
            .unwrap()
            .to_raw(),
    };
    let mut actions = iced_runtime::task::into_stream(bridge.dispatch(request(
        accesskit::Action::SetTextSelection,
        Some(accesskit::ActionData::SetTextSelection(selection)),
    )))
    .unwrap();
    while let Some(action) = iced::futures::executor::block_on(actions.next()) {
        match action {
            iced_runtime::Action::Widget(mut operation) => loop {
                input.operate(&mut tree, Layout::new(&node), &renderer, operation.as_mut());
                match operation.finish() {
                    iced::advanced::widget::operation::Outcome::Chain(next) => operation = next,
                    _ => break,
                }
            },
            iced_runtime::Action::Output(Message::ConnectAddress) => {
                panic!("native selection submitted Connect")
            }
            _ => {}
        }
    }
    let (selected, _, _) =
        accessibility::snapshot_widget(&mut input, &mut tree, Layout::new(&node), &renderer);
    let consumer = accesskit_consumer::Tree::new(selected, true);
    assert_eq!(
        consumer
            .state()
            .node_by_id(id)
            .unwrap()
            .text_selection()
            .unwrap()
            .text(),
        target
    );
    let mut messages = Vec::new();
    input.update(
        &mut tree,
        &Event::Keyboard(keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(keyboard::key::Named::Enter),
            modified_key: keyboard::Key::Named(keyboard::key::Named::Enter),
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Enter),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: None,
            repeat: false,
        }),
        Layout::new(&node),
        mouse::Cursor::Unavailable,
        &renderer,
        &mut iced::advanced::clipboard::Null,
        &mut iced::advanced::Shell::new(&mut messages),
        &iced::Rectangle::with_size(Size::new(480.0, 40.0)),
    );
    assert!(
        matches!(messages.as_slice(), [Message::ConnectAddress]),
        "ordinary keyboard Return retains the single submit action"
    );
}
