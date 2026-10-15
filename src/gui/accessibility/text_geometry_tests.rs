use super::super::*;
use iced::advanced::widget::operation::scrollable::{AbsoluteOffset, RelativeOffset, Scrollable};

struct ScrollState;
impl Scrollable for ScrollState {
    fn snap_to(&mut self, _: RelativeOffset<Option<f32>>) {}
    fn scroll_to(&mut self, _: AbsoluteOffset<Option<f32>>) {}
    fn scroll_by(&mut self, _: AbsoluteOffset, _: Rectangle, _: Rectangle) {}
}

#[test]
fn collector_rebases_real_consumer_text_geometry_through_scroll_clip_and_scale_once() {
    for scale in [1.0, 1.3, 2.0] {
        for rtl in [false, true] {
            let mut collector = Collector::new(scale);
            let mut field = Control::<Message>::new(Role::TextInput);
            field.node.set_label("Field");
            field.node.set_value("abcd");
            let text_id = new_node_id();
            let mut run = Node::new(Role::TextRun);
            run.set_value("abcd");
            run.set_character_lengths([1, 1, 1, 1]);
            run.set_character_positions([0.0, 10.0, 20.0, 30.0]);
            run.set_character_widths([10.0; 4]);
            run.set_text_direction(if rtl {
                accesskit::TextDirection::RightToLeft
            } else {
                accesskit::TextDirection::LeftToRight
            });
            run.set_bounds(Rect {
                x0: 100.0,
                y0: 80.0,
                x1: 140.0,
                y1: 100.0,
            });
            field.node.set_children([text_id]);
            field.children = vec![(text_id, run)];
            collector.scrollable(
                None,
                Rectangle {
                    x: 105.0,
                    y: 10.0,
                    width: 25.0,
                    height: 20.0,
                },
                Rectangle {
                    x: 0.0,
                    y: 0.0,
                    width: 300.0,
                    height: 200.0,
                },
                Vector::new(5.0, 60.0),
                &mut ScrollState,
            );
            collector.traverse(&mut |operation| {
                operation.custom(
                    None,
                    Rectangle {
                        x: 100.0,
                        y: 80.0,
                        width: 40.0,
                        height: 20.0,
                    },
                    &mut field,
                );
            });
            let (snapshot, _) = collector.finish(
                "Text geometry",
                Rectangle {
                    x: 0.0,
                    y: 0.0,
                    width: 300.0,
                    height: 200.0,
                },
            );
            let run = &snapshot
                .nodes
                .iter()
                .find(|(id, _)| *id == text_id)
                .unwrap()
                .1;
            let expected = Rect {
                x0: 105.0 * scale as f64,
                y0: 20.0 * scale as f64,
                x1: 130.0 * scale as f64,
                y1: 30.0 * scale as f64,
            };
            let actual = run.bounds().unwrap();
            for (actual, expected) in [
                (actual.x0, expected.x0),
                (actual.y0, expected.y0),
                (actual.x1, expected.x1),
                (actual.y1, expected.y1),
            ] {
                assert!((actual - expected).abs() < 0.0001);
            }
            let expected_positions = if rtl {
                [0.0, 5.0, 15.0, 25.0]
            } else {
                [0.0, 0.0, 10.0, 20.0]
            };
            let expected_widths = if rtl {
                [5.0, 10.0, 10.0, 0.0]
            } else {
                [0.0, 10.0, 10.0, 5.0]
            };
            for (actual, logical) in run
                .character_positions()
                .unwrap()
                .iter()
                .zip(expected_positions)
            {
                assert!((*actual - logical * scale).abs() < 0.0001);
            }
            for (actual, logical) in run.character_widths().unwrap().iter().zip(expected_widths) {
                assert!((*actual - logical * scale).abs() < 0.0001);
            }
            let consumer = accesskit_consumer::Tree::new(snapshot, true);
            let input = consumer.state().node_by_id(field.id).unwrap();
            assert_eq!(input.document_range().text(), "abcd");
            assert_eq!(input.document_range().bounding_boxes().len(), 1);
            for (index, expected_width) in expected_widths.iter().enumerate() {
                let mut range = input.document_range();
                range.set_start(input.text_position_from_global_utf16_index(index).unwrap());
                range.set_end(
                    input
                        .text_position_from_global_utf16_index(index + 1)
                        .unwrap(),
                );
                assert_eq!(range.text(), &"abcd"[index..index + 1]);
                let boxes = range.bounding_boxes();
                assert_eq!(boxes.len(), 1);
                let rect = boxes[0];
                assert!(rect.x0 >= expected.x0 - 0.0001 && rect.x1 <= expected.x1 + 0.0001);
                assert!(rect.y0 >= expected.y0 - 0.0001 && rect.y1 <= expected.y1 + 0.0001);
                assert!((rect.width() - *expected_width as f64 * scale as f64).abs() < 0.0001);
                if rect.width() > 0.001 {
                    let hit = input.text_position_at_point(accesskit::Point::new(
                        rect.x0 + rect.width() * 0.5,
                        rect.y0 + rect.height() * 0.5,
                    ));
                    assert_eq!(
                        hit.to_global_utf16_index(),
                        index,
                        "native hit follows clipped {} text at scale {scale}",
                        if rtl { "RTL" } else { "LTR" }
                    );
                }
            }
        }
    }
}

#[test]
fn explicit_bounds_without_character_geometry_keep_text_selection_and_scaled_clip() {
    let mut collector = Collector::new(1.3);
    let mut field = Control::<Message>::new(Role::TextInput);
    let text_id = new_node_id();
    let selection = accesskit::TextSelection {
        anchor: accesskit::TextPosition {
            node: text_id,
            character_index: 0,
        },
        focus: accesskit::TextPosition {
            node: text_id,
            character_index: 1,
        },
    };
    field.node.set_value("e\u{301}");
    field.node.set_text_selection(selection);
    field.node.set_children([text_id]);
    let mut run = Node::new(Role::TextRun);
    run.set_value("e\u{301}");
    run.set_character_lengths([3]);
    run.set_bounds(Rect {
        x0: 100.0,
        y0: 80.0,
        x1: 140.0,
        y1: 100.0,
    });
    field.children = vec![(text_id, run)];
    collector.scrollable(
        None,
        Rectangle {
            x: 105.0,
            y: 10.0,
            width: 25.0,
            height: 20.0,
        },
        Rectangle {
            x: 0.0,
            y: 0.0,
            width: 300.0,
            height: 200.0,
        },
        Vector::new(5.0, 60.0),
        &mut ScrollState,
    );
    collector.traverse(&mut |operation| {
        operation.custom(
            None,
            Rectangle {
                x: 100.0,
                y: 80.0,
                width: 40.0,
                height: 20.0,
            },
            &mut field,
        )
    });
    let (snapshot, _) = collector.finish(
        "Fallback geometry",
        Rectangle::with_size(Size::new(300.0, 200.0)),
    );
    let run = &snapshot
        .nodes
        .iter()
        .find(|(id, _)| *id == text_id)
        .unwrap()
        .1;
    assert_eq!(run.value(), Some("e\u{301}"));
    assert_eq!(run.character_lengths(), &[3]);
    assert!(run.character_positions().is_none());
    assert!(run.character_widths().is_none());
    assert!(run.text_direction().is_none());
    let bounds = run.bounds().unwrap();
    for (actual, expected) in [
        (bounds.x0, 105.0),
        (bounds.x1, 130.0),
        (bounds.y0, 20.0),
        (bounds.y1, 30.0),
    ] {
        assert!((actual - expected * 1.3_f32 as f64).abs() < 0.0001);
    }
    let consumer = accesskit_consumer::Tree::new(snapshot, true);
    let input = consumer.state().node_by_id(field.id).unwrap();
    assert_eq!(input.document_range().text(), "e\u{301}");
    assert_eq!(input.text_selection().unwrap().text(), "e\u{301}");
    assert_eq!(
        input
            .text_selection_anchor()
            .unwrap()
            .to_global_utf16_index(),
        0
    );
    assert_eq!(
        input
            .text_selection_focus()
            .unwrap()
            .to_global_utf16_index(),
        2
    );
    assert!(
        input.document_range().bounding_boxes().is_empty(),
        "missing source geometry must not be fabricated"
    );
}

#[test]
fn wholly_clipped_text_runs_preserve_text_without_native_point_exposure() {
    use accesskit_consumer::FilterResult;
    for root_clip in [false, true] {
        let mut collector = Collector::new(2.0);
        let mut field = Control::<Message>::new(Role::TextInput);
        let text_id = new_node_id();
        field.node.set_value("abc");
        field.node.set_text_selection(accesskit::TextSelection {
            anchor: accesskit::TextPosition {
                node: text_id,
                character_index: 0,
            },
            focus: accesskit::TextPosition {
                node: text_id,
                character_index: 3,
            },
        });
        field.node.set_children([text_id]);
        let raw = Rectangle {
            x: 100.0,
            y: 130.0,
            width: 30.0,
            height: 20.0,
        };
        let mut run = Node::new(Role::TextRun);
        run.set_value("abc");
        run.set_character_lengths([1, 1, 1]);
        run.set_character_positions([0.0, 10.0, 20.0]);
        run.set_character_widths([10.0; 3]);
        run.set_text_direction(accesskit::TextDirection::LeftToRight);
        run.set_bounds(rect(raw, 1.0));
        field.children = vec![(text_id, run)];
        if root_clip {
            collector
                .viewport
                .root(Rectangle::with_size(Size::new(50.0, 50.0)));
            collector.custom(None, raw, &mut field);
        } else {
            collector.scrollable(
                None,
                Rectangle {
                    x: 105.0,
                    y: 10.0,
                    width: 25.0,
                    height: 20.0,
                },
                Rectangle::with_size(Size::new(300.0, 200.0)),
                Vector::new(5.0, 60.0),
                &mut ScrollState,
            );
            collector.traverse(&mut |operation| operation.custom(None, raw, &mut field));
        }
        let (snapshot, _) = collector.finish(
            "Clipped text",
            Rectangle::with_size(Size::new(300.0, 200.0)),
        );
        let bounds = snapshot
            .nodes
            .iter()
            .find(|(id, _)| *id == text_id)
            .unwrap()
            .1
            .bounds()
            .unwrap();
        assert!(bounds.is_empty());
        let consumer = accesskit_consumer::Tree::new(snapshot, true);
        let input = consumer.state().node_by_id(field.id).unwrap();
        assert_eq!(input.document_range().text(), "abc");
        assert_eq!(input.text_selection().unwrap().text(), "abc");
        assert!(
            input
                .document_range()
                .bounding_boxes()
                .iter()
                .all(Rect::is_empty)
        );
        let root = consumer.state().node_by_id(ROOT).unwrap();
        // Include even the exact zero-area boundary: AccessKit's half-open
        // rectangle rule cannot expose an invisible run at a clipped edge.
        for point in [
            accesskit::Point::new(bounds.x0, bounds.y0),
            accesskit::Point::new(210.0, 40.0),
            accesskit::Point::new(230.0, 280.0),
        ] {
            assert!(
                root.node_at_point(point, &|node| if node.role() == Role::TextRun {
                    FilterResult::Include
                } else {
                    FilterResult::ExcludeNode
                })
                .is_none()
            );
        }
    }
}
