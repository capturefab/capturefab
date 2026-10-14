//! Compact inspector geometry and real pointer access after scrolling.
use super::*;
use crate::gui::{
    accessibility,
    destinations::layout_tests::{
        assert_focus_traversal_reveals, prepare_artifact_frame, render_artifact_scaled, renderer,
    },
};
use iced::advanced::{
    Layout, layout,
    widget::{Id, Operation, Tree, operation::scrollable},
};
use iced::{Point, Rectangle, Size, Vector};

#[derive(Default)]
struct ScrollGeometry {
    bounds: Option<Rectangle>,
    content: Option<Rectangle>,
    translation: Vector,
    capture: Option<Rectangle>,
}
impl Operation for ScrollGeometry {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        operate(self);
    }
    fn custom(&mut self, _: Option<&Id>, bounds: Rectangle, state: &mut dyn std::any::Any) {
        if let Some(control) = state.downcast_ref::<accessibility::Control<Message>>()
            && matches!(control.activate.as_ref(), Some(Message::Capture))
        {
            self.capture = Some(bounds);
        }
    }
    fn scrollable(
        &mut self,
        id: Option<&Id>,
        bounds: Rectangle,
        content: Rectangle,
        translation: Vector,
        _: &mut dyn scrollable::Scrollable,
    ) {
        if id == Some(&Id::new(SCROLL)) {
            self.bounds = Some(bounds);
            self.content = Some(content);
            self.translation = translation;
        }
    }
}

fn bench() -> Workbench {
    let mut app = Workbench::new(SessionHandle::new(), "compact-layout".into(), true, None);
    let mut info = crate::transport::simulator::info();
    info.model = "Laboratory camera with a long acquisition and measurement model name".into();
    app.snapshot.connected = Some(info.clone());
    let mut current = liveness::streaming_camera(0, 0, 0.0);
    current.info = info.clone();
    let mut other = current.clone();
    other.info.id = "sim:1".into();
    app.snapshot.cameras = vec![current, other];
    app.snapshot.active_camera = Some(info.id);
    app.output = "/tmp/laboratory-camera-captures/a-long-capture-directory-name/capture.png".into();
    app.schedule_enabled = true;
    app.storage_open = true;
    app.retention_enabled = true;
    let mut feature = sample_feature("ExposureTime", "Float", serde_json::json!(10000.0));
    feature.display_name = "Exposure time for the selected acquisition measurement channel".into();
    feature.unit = Some("µs".into());
    feature.description = "A long camera description explains which measurement channel is affected by this exposure setting.".into();
    app.snapshot.features = vec![feature];
    app.tick_inspector();
    app
}

#[test]
fn compact_inspector_camera_context_retains_identity_beside_a_long_model_name() {
    let renderer = renderer();
    let app = bench();
    let mut view = app.camera_context(Palette::of(false)).unwrap();
    let mut tree = Tree::new(view.as_widget());
    let node = view.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, Size::new(INSPECTOR, 80.0)),
    );
    let row = &node.children()[0].children()[0];
    assert_eq!(row.children().len(), 3);
    assert!(
        row.children()[2].size().width >= 60.0,
        "the selected camera's identity remains readable beside its long model name"
    );
}

#[test]
fn compact_inspector_tab_and_shift_tab_reveal_the_actual_workflow_controls() {
    let renderer = renderer();
    for scale in prefs::UiScale::ALL {
        let size = Size::new(INSPECTOR, 360.0 / scale.factor() - BAR - TOOLBAR);
        let mut app = bench();
        for tab in [Tab::Features, Tab::Capture, Tab::Forward] {
            app.tab = tab;
            app.inspect.tab_thumb.set(tab as usize as f32);
            let mut view = app.inspector(Palette::of(false));
            let mut tree = Tree::new(view.as_widget());
            let node = view.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, size),
            );
            assert_focus_traversal_reveals(&mut view, &mut tree, &node, &renderer);
        }
    }
}

#[test]
fn compact_inspector_tabs_keep_their_scroll_region_and_capture_action_reachable() {
    let mut renderer = renderer();
    for native_height in [620.0, 518.0, 480.0, 360.0] {
        for scale in prefs::UiScale::ALL {
            let mut app = bench();
            app.ui_scale = scale;
            let size = Size::new(INSPECTOR, native_height / scale.factor() - BAR - TOOLBAR);
            for tab in [Tab::Features, Tab::Capture, Tab::Forward] {
                app.tab = tab;
                app.inspect.tab_thumb.set(tab as usize as f32);
                let mut view = app.inspector(Palette::of(false));
                let mut tree = Tree::new(view.as_widget());
                let node = view.as_widget_mut().layout(
                    &mut tree,
                    &renderer,
                    &layout::Limits::new(Size::ZERO, size),
                );
                let mut scroll = ScrollGeometry::default();
                view.as_widget_mut()
                    .operate(&mut tree, Layout::new(&node), &renderer, &mut scroll);
                let bounds = scroll
                    .bounds
                    .expect("each connected inspector tab has a scroll viewport");
                assert!(bounds.height >= 80.0 && bounds.y + bounds.height <= size.height + 0.5);
                let (snapshot, _, _) = accessibility::snapshot_widget(
                    view.as_widget_mut(),
                    &mut tree,
                    Layout::new(&node),
                    &renderer,
                );
                for (_, control) in snapshot.nodes.iter().filter(|(_, node)| {
                    matches!(
                        node.role(),
                        accessibility::Role::Button
                            | accessibility::Role::TextInput
                            | accessibility::Role::ComboBox
                            | accessibility::Role::CheckBox
                    )
                }) {
                    let bound = control.bounds().unwrap();
                    assert!(
                        bound.x0 >= -0.5 && bound.x1 <= f64::from(INSPECTOR) + 0.5,
                        "{} extends beyond inspector width at {tab:?}, {scale}: {bound:?}",
                        control.label().unwrap_or("control")
                    );
                }
                if tab == Tab::Capture {
                    let primary = snapshot
                        .nodes
                        .iter()
                        .find(|(_, node)| {
                            node.role() == accessibility::Role::Button
                                && node
                                    .label()
                                    .is_some_and(|label| label.starts_with("Capture and save"))
                        })
                        .expect("actual capture action");
                    assert!(primary.1.supports_action(accesskit::Action::Click));
                    let original = scroll.capture.expect("raw capture button layout bounds");
                    let offset = ((original.y - bounds.y - 8.0).max(0.0))
                        .min((scroll.content.unwrap().height - bounds.height).max(0.0));
                    let mut to = scrollable::scroll_to::<()>(
                        Id::new(SCROLL),
                        scrollable::AbsoluteOffset {
                            x: None,
                            y: Some(offset),
                        },
                    );
                    view.as_widget_mut()
                        .operate(&mut tree, Layout::new(&node), &renderer, &mut to);
                    let mut after = ScrollGeometry::default();
                    view.as_widget_mut().operate(
                        &mut tree,
                        Layout::new(&node),
                        &renderer,
                        &mut after,
                    );
                    assert!(after.translation.y >= 0.0);
                    let cursor = iced::mouse::Cursor::Available(Point::new(
                        original.center_x(),
                        original.center_y() - after.translation.y,
                    ));
                    assert!(bounds.contains(cursor.position().unwrap()));
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
                            &Rectangle::new(Point::ORIGIN, size),
                        );
                    }
                    assert!(
                        messages
                            .iter()
                            .any(|message| matches!(message, Message::Capture)),
                        "the actual capture button responds after scrolling at native height {native_height}, {scale}"
                    );
                }
                prepare_artifact_frame(&mut view, &mut tree, &node, &renderer, size);
                render_artifact_scaled(
                    &format!("inspector-{tab:?}-{native_height}-{scale}"),
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
