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
    app.sidebar_open = true;
    app.sidebar_slide.set(1.0);
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
            for overview in [false, true] {
                let mut app = bench();
                app.width = native_width / scale.factor();
                app.ui_scale = scale;
                app.focus_camera = !overview;
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
                view.as_widget_mut().operate(
                    &mut tree,
                    Layout::new(&node),
                    &renderer,
                    &mut geometry,
                );
                for (label, bounds) in &geometry.controls {
                    assert!(
                        inside(*bounds, viewport),
                        "{label} clips at native width {native_width}, {scale}, overview={overview}: {bounds:?}, viewport={viewport:?}",
                    );
                }
                let capture = if overview {
                    "Capture selected"
                } else {
                    "Capture"
                };
                let destination =
                    format!("Capture destination: {}", app.capture_to.label(&app.output));
                for action in [
                    capture,
                    destination.as_str(),
                    "Session analytics for all cameras",
                    "Session activity",
                    "Show activity details",
                    "Dismiss notification",
                ] {
                    let (_, bounds) = geometry.controls.iter().find(|(label, _)| label == action)
                        .unwrap_or_else(|| panic!("{action} is available at {native_width}, {scale}, overview={overview}"));
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
                            "Capture" | "Capture selected" => matches!(message, Message::Capture),
                            "Session analytics for all cameras" =>
                                matches!(message, Message::Analytics(true)),
                            "Session activity" => matches!(message, Message::ToggleActivity),
                            "Show activity details" => matches!(message, Message::ShowActivity),
                            "Dismiss notification" => matches!(message, Message::DismissNotice),
                            _ if action == destination => {
                                matches!(message, Message::ShowTab(Tab::Capture))
                            }
                            _ => false,
                        }),
                        "{action} responds at {native_width}, {scale}, overview={overview}"
                    );
                }
                let (_, capture_bounds) = geometry
                    .controls
                    .iter()
                    .find(|(label, _)| label == capture)
                    .unwrap();
                let (_, capture_text) = geometry
                    .labels
                    .iter()
                    .find(|(label, _)| label == capture)
                    .unwrap();
                assert!(
                    inside(*capture_text, *capture_bounds),
                    "the complete Capture label remains readable"
                );
                prepare_artifact_frame(&mut view, &mut tree, &node, &renderer, size);
                render_artifact_scaled(
                    &format!("toolbar-{native_width}-{scale}-overview-{overview}"),
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
}

#[test]
fn responsive_toolbar_reveals_labels_before_duplicate_edit_actions() {
    let renderer = renderer();
    for (width, labels, history) in [
        (544.0, false, false),
        (720.0, true, false),
        (960.0, true, true),
    ] {
        let mut app = bench();
        app.width = width;
        app.sidebar_slide.set(0.0);
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
fn resizing_preserves_capture_and_session_action_identity() {
    let renderer = renderer();
    let mut app = bench();
    app.focus_camera = true;
    app.sidebar_slide.set(0.0);
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
