//! The real toolbar must preserve complete capture actions when panes and
//! larger interface text reduce the space beneath the preview.
use super::*;
use crate::gui::destinations::layout_tests::{
    prepare_artifact_frame, render_artifact_scaled, renderer,
};
use iced::Rectangle;
use iced::advanced::{
    Layout, layout,
    widget::{Id, Operation, Tree},
};

#[derive(Default)]
struct Geometry {
    controls: Vec<(String, Rectangle)>,
    labels: Vec<(String, Rectangle)>,
}

impl Operation for Geometry {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        operate(self);
    }
    fn text(&mut self, _: Option<&Id>, bounds: Rectangle, text: &str) {
        self.labels.push((text.to_owned(), bounds));
    }
    fn custom(&mut self, _: Option<&Id>, bounds: Rectangle, state: &mut dyn std::any::Any) {
        if let Some(control) = state.downcast_ref::<accessibility::Control<Message>>()
            && control.node.role() == accessibility::Role::Button
            && let Some(label) = control.node.label()
        {
            self.controls.push((label.to_owned(), bounds));
        }
    }
}

fn bench() -> Workbench {
    let mut app = Workbench::new(SessionHandle::new(), "toolbar-layout".into(), true, None);
    let camera = liveness::streaming_camera(0, 0, 0.0);
    let mut other = camera.clone();
    other.info.id = "sim:1".into();
    app.snapshot.active_camera = Some(camera.info.id.clone());
    app.snapshot.connected = Some(camera.info.clone());
    app.snapshot.cameras = vec![camera, other];
    app.output = "/tmp/long-laboratory-capture-destination/a-descriptive-camera-name.png".into();
    app.inspector_open = true;
    app.inspector_slide.set(1.0);
    app.put_notice(
        "Capture failed: the destination no longer has enough free space".into(),
        Some("Choose another destination, then capture again.".into()),
        Level::Error,
    );
    app.notice_shown.set(1.0);
    app
}

fn inside(inner: Rectangle, outer: Rectangle) -> bool {
    inner.width > 0.0
        && inner.height > 0.0
        && inner.x >= outer.x - 0.5
        && inner.y >= outer.y - 0.5
        && inner.x + inner.width <= outer.x + outer.width + 0.5
        && inner.y + inner.height <= outer.y + outer.height + 0.5
}

#[test]
fn responsive_toolbar_keeps_capture_and_session_actions_visible_and_clickable() {
    let mut renderer = renderer();
    for native_width in [900.0, 1100.0, 1280.0, 1600.0] {
        for scale in prefs::UiScale::ALL {
            let mut app = bench();
            app.width = native_width / scale.factor();
            app.ui_scale = scale;
            let size = Size::new(app.main_width(), TOOLBAR);
            let viewport = Rectangle::with_size(size);
            let mut view = app.toolbar(Palette::of(false));
            let mut tree = Tree::new(view.as_widget());
            let node = view.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, size),
            );
            let mut geometry = Geometry::default();
            view.as_widget_mut()
                .operate(&mut tree, Layout::new(&node), &renderer, &mut geometry);
            for (label, bounds) in &geometry.controls {
                assert!(
                    inside(*bounds, viewport),
                    "{label} clips at native width {native_width}, {scale}: {bounds:?}, viewport={viewport:?}",
                );
            }
            let destination = format!("Capture destination: {}", app.capture_to.label(&app.output));
            for action in [
                "Capture",
                "Capture All",
                destination.as_str(),
                "Session analytics for all cameras",
                "Resource usage and limits",
                "Session activity",
                "Show activity details",
                "Dismiss notification",
            ] {
                let (_, bounds) = geometry
                    .controls
                    .iter()
                    .find(|(label, _)| label == action)
                    .unwrap_or_else(|| panic!("{action} is available at {native_width}, {scale}"));
                let cursor = iced::mouse::Cursor::Available(bounds.center());
                let mut messages = Vec::new();
                for event in [
                    iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left),
                    iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left),
                ] {
                    view.as_widget_mut().update(
                        &mut tree,
                        &iced::Event::Mouse(event),
                        Layout::new(&node),
                        cursor,
                        &renderer,
                        &mut iced::advanced::clipboard::Null,
                        &mut iced::advanced::Shell::new(&mut messages),
                        &viewport,
                    );
                }
                assert!(
                    messages.iter().any(|message| match action {
                        "Capture" => matches!(message, Message::Capture),
                        "Capture All" => matches!(message, Message::AllCapture),
                        "Session analytics for all cameras" =>
                            matches!(message, Message::Analytics(true)),
                        "Resource usage and limits" =>
                            matches!(message, Message::Resources(resources::Input::Open(true))),
                        "Session activity" => matches!(message, Message::ToggleActivity),
                        "Show activity details" => matches!(message, Message::ShowActivity),
                        "Dismiss notification" => matches!(message, Message::DismissNotice),
                        _ if action == destination => {
                            matches!(message, Message::ShowTab(Tab::Capture))
                        }
                        _ => false,
                    }),
                    "{action} responds at {native_width}, {scale}"
                );
            }
            let (_, capture_bounds) = geometry
                .controls
                .iter()
                .find(|(label, _)| label == "Capture")
                .unwrap();
            let (_, capture_text) = geometry
                .labels
                .iter()
                .find(|(label, _)| *label == "Capture")
                .unwrap_or_else(|| panic!("Capture is whole at {native_width}, {scale}"));
            assert!(
                inside(*capture_text, *capture_bounds),
                "the complete Capture label remains readable"
            );
            prepare_artifact_frame(&mut view, &mut tree, &node, &renderer, size);
            render_artifact_scaled(
                &format!("toolbar-{native_width}-{scale}"),
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
fn a_timed_recording_counts_down_beside_its_stop_button() {
    let renderer = renderer();
    for native_width in [900.0, 1280.0] {
        for scale in prefs::UiScale::ALL {
            let mut app = bench();
            app.width = native_width / scale.factor();
            app.ui_scale = scale;
            // Two hours from 1:23 ago: the widest time left, "1:58:37 left".
            let since = epoch_ms().saturating_sub(83_000);
            app.snapshot.forwarding = Some("/tmp/capturefab-clip.mp4".into());
            app.snapshot.forwarding_since_ms = Some(since);
            app.snapshot.forwarding_until_ms = Some(since + 7_200_000);
            let size = Size::new(app.main_width(), TOOLBAR);
            let viewport = Rectangle::with_size(size);
            let mut view = app.toolbar(Palette::of(false));
            let mut tree = Tree::new(view.as_widget());
            let node = view.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, size),
            );
            let mut geometry = Geometry::default();
            view.as_widget_mut()
                .operate(&mut tree, Layout::new(&node), &renderer, &mut geometry);
            let at = format!("{native_width}, {scale}");
            let (_, stop) = geometry
                .controls
                .iter()
                .find(|(label, _)| label == "Stop recording")
                .unwrap_or_else(|| panic!("Stop recording is available at {at}"));
            let (left, pill) = geometry
                .labels
                .iter()
                .find(|(label, _)| label.ends_with(" left"))
                .unwrap_or_else(|| panic!("the time left shows at {at}: {:?}", geometry.labels));
            assert!(left.starts_with("1:58:"), "{left}");
            for (what, bounds) in [("Stop recording", stop), (left.as_str(), pill)] {
                assert!(
                    inside(*bounds, viewport),
                    "{what} clips at {at}: {bounds:?}"
                );
            }
            assert!(
                pill.x >= stop.x + stop.width,
                "the pill follows Stop at {at}"
            );
            let (native, _, _) = accessibility::snapshot_widget(
                view.as_widget_mut(),
                &mut tree,
                Layout::new(&node),
                &renderer,
            );
            assert!(
                native.nodes.iter().any(|(_, node)| {
                    node.label() == Some(left.as_str())
                        && node.description().is_some_and(|text| {
                            text.starts_with("Records for 2 h; stops by itself at")
                        })
                }),
                "the time left is named and says when the recording stops"
            );
        }
    }
}

#[test]
fn responsive_toolbar_reveals_labels_before_duplicate_edit_actions() {
    let renderer = renderer();
    for (width, labels, history) in [
        (544.0, false, false),
        (760.0, true, false),
        (960.0, true, true),
    ] {
        let mut app = bench();
        app.width = width;
        app.inspector_slide.set(0.0);
        let mut view = app.toolbar(Palette::of(false));
        let mut tree = Tree::new(view.as_widget());
        let node = view.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(width, TOOLBAR)),
        );
        let mut geometry = Geometry::default();
        view.as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut geometry);
        assert_eq!(
            geometry.labels.iter().any(|(label, _)| label == "Activity"),
            labels
        );
        assert_eq!(
            geometry.controls.iter().any(|(label, _)| label == "Undo"),
            history
        );
        assert_eq!(
            geometry.controls.iter().any(|(label, _)| label == "Redo"),
            history
        );
        let (native, _, _) = accessibility::snapshot_widget(
            view.as_widget_mut(),
            &mut tree,
            Layout::new(&node),
            &renderer,
        );
        let activity = native
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some("Session activity"))
            .unwrap();
        assert_eq!(activity.1.is_expanded(), Some(false));
    }
}

#[test]
fn a_problem_notice_keeps_room_to_say_what_went_wrong() {
    let renderer = renderer();
    for main in [650.0, 760.0, 1100.0] {
        let mut app = bench();
        app.inspector_slide.set(0.0);
        app.inspector_open = false;
        app.width = main;
        app.put_notice(
            "Could not connect: no GigE Vision camera answered at 192.168.1.20".into(),
            None,
            Level::Error,
        );
        let size = Size::new(app.main_width(), TOOLBAR);
        let mut view = app.toolbar(Palette::of(false));
        let mut tree = Tree::new(view.as_widget());
        let node = view.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, size),
        );
        let mut geometry = Geometry::default();
        view.as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut geometry);
        let (_, text) = geometry
            .labels
            .iter()
            .find(|(label, _)| label.starts_with("Could not connect"))
            .unwrap_or_else(|| panic!("{main}: {:?}", geometry.labels));
        assert!(text.width >= 100.0, "{main}: {text:?}");
    }
}

#[test]
fn the_welcome_toolbar_offers_no_dead_undo_or_redo() {
    let renderer = renderer();
    let mut app = Workbench::new(SessionHandle::new(), "toolbar-welcome".into(), true, None);
    app.width = 1200.0;
    app.inspector_slide.set(0.0);
    app.inspector_open = false;
    assert!(app.snapshot.connected.is_none() && app.history_label(true).is_none());
    let mut view = app.toolbar(Palette::of(false));
    let mut tree = Tree::new(view.as_widget());
    let node = view.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, Size::new(app.main_width(), TOOLBAR)),
    );
    let mut geometry = Geometry::default();
    view.as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut geometry);
    assert!(
        !geometry
            .controls
            .iter()
            .any(|(label, _)| label == "Undo" || label == "Redo"),
        "{:?}",
        geometry.controls
    );
}

#[test]
fn resizing_preserves_capture_and_session_action_identity() {
    let renderer = renderer();
    let mut app = bench();
    app.inspector_slide.set(0.0);
    let mut tree = Tree::empty();
    let mut previous = None;
    for width in [544.0, 960.0, 450.0, 720.0, 544.0] {
        app.width = width;
        let mut view = app.toolbar(Palette::of(false));
        tree.diff(view.as_widget());
        let node = view.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(width, TOOLBAR)),
        );
        let (native, _, _) = accessibility::snapshot_widget(
            view.as_widget_mut(),
            &mut tree,
            Layout::new(&node),
            &renderer,
        );
        let identities = [
            "Capture",
            "Capture All",
            "Session analytics for all cameras",
            "Session activity",
        ]
        .map(|label| {
            native
                .nodes
                .iter()
                .find(|(_, node)| node.label() == Some(label))
                .unwrap()
                .0
        });
        if let Some(previous) = previous {
            assert_eq!(identities, previous, "resizing preserves action identity");
        }
        previous = Some(identities);
    }
}
