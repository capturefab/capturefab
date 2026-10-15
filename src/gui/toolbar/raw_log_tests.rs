//! Raw messages and structured commands use the real compact Activity content.
//! Shaped paragraphs must fit beside timestamps/marks and before the scrollbar.
use super::*;
use iced::advanced::{
    Layout, layout,
    text::Paragraph,
    widget::{Id, Operation, Tree},
};

type TextState =
    iced::widget::text::State<<iced::Renderer as iced::advanced::text::Renderer>::Paragraph>;

#[derive(Default)]
struct Texts(Vec<String>);
impl Operation for Texts {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        operate(self);
    }
    fn text(&mut self, _: Option<&Id>, _: iced::Rectangle, text: &str) {
        self.0.push(text.to_owned());
    }
}

#[test]
fn raw_and_structured_activity_rows_wrap_before_the_overlay_at_every_interface_size() {
    let mut renderer = crate::gui::destinations::layout_tests::renderer();
    let mut app = Workbench::new(SessionHandle::new(), "raw-log-wrap".into(), true, None);
    app.snapshot.logs = vec![
        crate::session::LogEntry {
            timestamp: None,
            time: "12:34:56".into(), level: "warn".into(),
            message: "Scanner discovery: timeout waiting for scanner helper. Check the device connection and retry discovery after the helper becomes available. ".repeat(4),
        },
        crate::session::LogEntry {
            timestamp: None,
            time: String::new(), level: "error".into(),
            message: format!("Could not open /isolated/{}/capture.png", "unbroken".repeat(64)),
        },
    ];
    let detail = format!("/isolated/{}/capture.png", "unbroken".repeat(64));
    app.snapshot.activity = vec![serde_json::from_value(serde_json::json!({
        "id": 77, "origin": "client", "actor": {
            "id": "agent-unbroken".repeat(8), "name": "Workflow agent", "kind": "agent"
        }, "operation": "capture", "camera": "sim:0", "feature": null,
        "label": "Capture failed: output folder is read-only", "state": "failed", "submitted_at_ms": 1,
        "started_at_ms": 1, "finished_at_ms": 2, "detail": detail,
        "file_count": 0, "files": [], "job": null
    })).unwrap()];
    let mut tree = Tree::empty();
    for scale in prefs::UiScale::ALL {
        for native_width in [480.0, 640.0, 900.0] {
            let size = Size::new(native_width / scale.factor(), 171.0);
            let mut view = app.activity(Palette::of(false));
            tree.diff(view.as_widget());
            let node = view.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, size),
            );
            // The actual Activity column contains its header and scroller.
            // KeyboardScroll delegates the layout but owns one child tree.
            let panel = Layout::new(&node)
                .children()
                .nth(1)
                .unwrap()
                .children()
                .next()
                .unwrap();
            let scroller = panel.children().nth(1).unwrap();
            let rows = scroller.children().next().unwrap();
            let section_trees = &tree.children[0].children[1].children[1].children[0].children;
            assert_eq!(section_trees.len(), 2);
            let diagnostics = rows.children().next().unwrap();
            let row_trees = &section_trees[0].children;
            assert_eq!(row_trees.len(), app.snapshot.logs.len());
            for (index, (dated_row, dated_tree)) in
                diagnostics.children().zip(row_trees).enumerate()
            {
                let row = dated_row.children().nth(1).unwrap();
                let row_tree = &dated_tree.children[1];
                let message = row.children().nth(2).unwrap();
                let state = row_tree.children[2].state.downcast_ref::<TextState>();
                let measured = state.raw().min_bounds();
                let room = scroller.bounds().x + scroller.bounds().width - message.bounds().x;
                assert!(
                    message.bounds().width <= room + 0.01,
                    "raw log {index} layout exceeds its remaining width at {native_width}/{scale}: {:?}, room {room}",
                    message.bounds()
                );
                assert!(
                    measured.width <= message.bounds().width + 0.01,
                    "raw log {index} shaped text exceeds its allocated width at {native_width}/{scale}: {measured:?}, bounds {:?}",
                    message.bounds()
                );
                assert!(
                    message.bounds().x + message.bounds().width
                        <= scroller.bounds().x + scroller.bounds().width - 10.0,
                    "raw log text leaves the overlay scrollbar's 10-unit rail unobstructed"
                );
                assert!(
                    measured.height <= message.bounds().height + 0.01,
                    "every wrapped line remains in the raw message's layout"
                );
                assert!(
                    measured.height > style::line_height(style::CAPTION) * 2.0,
                    "the full long message wraps rather than being clipped or elided"
                );
            }
            let commands = rows.children().nth(1).unwrap();
            let dated_command = commands.children().next().unwrap();
            let command = dated_command.children().nth(1).unwrap();
            let body = command.children().nth(2).unwrap();
            let body_tree = &section_trees[1].children[0].children[1].children[0].children[2];
            for (text, text_tree) in body.children().zip(&body_tree.children) {
                let state = text_tree.state.downcast_ref::<TextState>();
                let measured = state.raw().min_bounds();
                assert!(
                    measured.width <= text.bounds().width + 0.01,
                    "structured actor/path text fits its allocated paragraph at {native_width}/{scale}"
                );
                assert!(
                    text.bounds().x + text.bounds().width
                        <= scroller.bounds().x + scroller.bounds().width - 10.0,
                    "structured actor/path text leaves the same overlay scrollbar gutter at {native_width}/{scale}"
                );
                assert!(
                    measured.height <= text.bounds().height + 0.01,
                    "all structured actor/path lines remain readable"
                );
                assert!(measured.height > style::line_height(style::CAPTION) * 1.5);
            }
            let mut texts = Texts::default();
            view.as_widget_mut()
                .operate(&mut tree, Layout::new(&node), &renderer, &mut texts);
            for entry in &app.snapshot.logs {
                assert_eq!(
                    texts
                        .0
                        .iter()
                        .filter(|text| text.as_str() == entry.message)
                        .count(),
                    1,
                    "the full worker message remains available exactly once"
                );
            }
            assert_eq!(
                texts.0.iter().filter(|text| **text == detail).count(),
                1,
                "the full structured output path remains available exactly once"
            );
            crate::gui::destinations::layout_tests::render_artifact_scaled(
                &format!("raw-activity-wrap-{native_width}-{scale}"),
                &view,
                &tree,
                &node,
                &mut renderer,
                size,
                scale.factor(),
            );
        }
    }
}

#[test]
fn local_date_boundaries_preserve_source_order_row_identity_and_the_pinned_copy_action() {
    use crate::session::{ActivityState, LogEntry, LogTimestamp, SessionActivity};
    use iced::advanced::widget::operation;
    use iced::futures::StreamExt;

    struct FocusFirst(bool);
    impl Operation for FocusFirst {
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
            operate(self);
        }
        fn focusable(
            &mut self,
            _: Option<&Id>,
            _: iced::Rectangle,
            state: &mut dyn operation::Focusable,
        ) {
            if !self.0 {
                state.focus();
                self.0 = true;
            }
        }
    }
    let renderer = crate::gui::destinations::layout_tests::renderer();
    let mut app = Workbench::new(SessionHandle::new(), "activity-dates".into(), true, None);
    let first = 1_792_017_000_123;
    let next = first + 86_400_000;
    let raw = |name: &str, instant| LogEntry {
        timestamp: LogTimestamp::new(instant, 14 * 3600),
        ..LogEntry::legacy("00:30:00", "info", name)
    };
    app.snapshot.logs = vec![
        raw("First diagnostic", first),
        raw("Retained diagnostic", first + 1),
        raw("Next-day diagnostic", next),
        LogEntry::legacy("01:23:45", "warn", "Legacy diagnostic"),
        raw("Earlier-day arrival", first + 2),
    ];
    let command = |id, submitted, finished| -> SessionActivity {
        serde_json::from_value(serde_json::json!({
            "id":id,"origin":"client","operation":"capture","camera":null,"feature":null,
            "label":format!("Command label {id}"),"state":"running","submitted_at_ms":submitted,
            "started_at_ms":submitted,"finished_at_ms":finished,"detail":null,"file_count":0,"files":[]
        })).unwrap()
    };
    app.snapshot.activity = vec![
        command(10, first as u64, None),
        command(11, first as u64, Some(next as u64)),
        command(12, u64::MAX, None),
    ];
    let date = time_format::log_day(&app.snapshot.logs[0]).label;
    let next_date = time_format::log_day(&app.snapshot.logs[2]).label;
    let mut tree = Tree::empty();
    let collect = |app: &Workbench, tree: &mut Tree, focus: bool| {
        let mut view = app.activity(Palette::of(false));
        tree.diff(view.as_widget());
        let node = view.as_widget_mut().layout(
            tree,
            &renderer,
            &layout::Limits::new(
                Size::ZERO,
                Size::new(480.0 / prefs::UiScale::Larger.factor(), 171.0),
            ),
        );
        if focus {
            view.as_widget_mut().operate(
                tree,
                Layout::new(&node),
                &renderer,
                &mut FocusFirst(false),
            );
        }
        accessibility::snapshot_widget(view.as_widget_mut(), tree, Layout::new(&node), &renderer)
    };
    let (initial, _, _) = collect(&app, &mut tree, true);
    let headings = |snapshot: &accesskit::TreeUpdate| {
        snapshot
            .nodes
            .iter()
            .filter(|(_, node)| node.role() == accessibility::Role::Heading)
            .map(|(_, node)| node.label().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        headings(&initial),
        [
            format!("Diagnostics · {date}"),
            format!("Diagnostics · {next_date}"),
            "Diagnostics · Date unavailable".into(),
            format!("Diagnostics · {date}"),
            format!("Command updates · {date}"),
            format!("Command updates · {next_date}"),
            "Command updates · Date unavailable".into(),
        ]
    );
    let named = |snapshot: &accesskit::TreeUpdate, name: &str| {
        snapshot
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some(name))
            .map(|(id, _)| *id)
            .unwrap()
    };
    let copy = named(&initial, "Copy activity log");
    assert_eq!(initial.focus, copy);
    let command_id = named(&initial, "Command 10");
    let command_heading = named(&initial, &format!("Command updates · {date}"));
    let retained_times: Vec<_> = app.snapshot.logs[1..]
        .iter()
        .map(|entry| {
            let full = time_format::log(entry).full;
            let id = initial
                .nodes
                .iter()
                .find(|(_, node)| node.description() == Some(full.as_str()))
                .unwrap()
                .0;
            (full, id)
        })
        .collect();
    let copy_before = app.activity_text();
    let positions: Vec<_> = app
        .snapshot
        .logs
        .iter()
        .map(|entry| copy_before.find(&entry.message).unwrap())
        .collect();
    assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
    assert!(!copy_before.contains("Diagnostics ·"));

    app.snapshot.logs.remove(0);
    app.snapshot.activity[0].state = ActivityState::Succeeded;
    app.snapshot.activity[0].finished_at_ms = Some(next as u64 + 1);
    app.sync_client_activity();
    let (updated, bridge, window) = collect(&app, &mut tree, false);
    assert_eq!(updated.focus, copy);
    assert_eq!(named(&updated, "Copy activity log"), copy);
    assert_eq!(named(&updated, "Command 10"), command_id);
    assert_eq!(
        named(&updated, &format!("Command updates · {next_date}")),
        command_heading
    );
    assert_eq!(
        headings(&updated)
            .iter()
            .filter(|heading| heading.starts_with("Command updates ·"))
            .count(),
        2
    );
    for (full, id) in retained_times {
        assert!(
            updated
                .nodes
                .iter()
                .any(|(now, node)| *now == id && node.description() == Some(full.as_str()))
        );
    }
    for (_, node) in updated
        .nodes
        .iter()
        .filter(|(_, node)| node.role() == accessibility::Role::Heading)
    {
        assert!(!node.supports_action(accesskit::Action::Focus));
        assert!(!node.supports_action(accesskit::Action::Click));
        assert!(node.live().is_none());
    }
    let ids: std::collections::HashSet<_> = updated.nodes.iter().map(|(id, _)| *id).collect();
    assert_eq!(
        ids.len(),
        updated.nodes.len(),
        "repeated dates must not duplicate native IDs"
    );
    let mut actions = iced_runtime::task::into_stream(bridge.dispatch(accessibility::Request {
        window,
        replay: None,
        action: accesskit::ActionRequest {
            action: accesskit::Action::Click,
            target: copy,
            data: None,
        },
    }))
    .unwrap();
    assert!(matches!(
        iced::futures::executor::block_on(actions.next()),
        Some(iced_runtime::Action::Output(Message::CopyLog))
    ));
    app.snapshot.logs.clear();
    app.snapshot.activity.clear();
    app.sync_client_activity();
    assert!(app.client_activity.raw_date_ids.borrow().is_empty());
    assert!(headings(&collect(&app, &mut tree, false).0).is_empty());
}
