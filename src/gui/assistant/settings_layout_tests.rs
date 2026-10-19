//! Exercise both real Settings presentations with the actual software renderer.
//! Moving through the form must reveal whole controls above the pinned footer.
use super::*;
use crate::gui::{
    accessibility,
    destinations::layout_tests::{
        assert_focus_traversal_reveals, prepare_artifact_frame, renderer,
    },
    prefs::UiScale,
};
use iced::advanced::{
    Layout, layout,
    renderer::Headless,
    widget::{Id, Operation, Tree, operation::scrollable},
};
use iced::{Point, Rectangle, Vector};

#[derive(Clone, Debug)]
struct ControlBounds {
    label: String,
    bounds: Rectangle,
    role: accessibility::Role,
}

#[derive(Default)]
struct SettingsGeometry {
    body: Option<Rectangle>,
    content: Option<Rectangle>,
    controls: Vec<ControlBounds>,
    live_regions: Vec<(accessibility::Role, Option<accesskit::Live>)>,
    scroll_to: Option<f32>,
    offset: Vector,
}

impl Operation for SettingsGeometry {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        operate(self);
    }

    fn scrollable(
        &mut self,
        _: Option<&Id>,
        bounds: Rectangle,
        content: Rectangle,
        offset: Vector,
        state: &mut dyn scrollable::Scrollable,
    ) {
        assert!(self.body.is_none(), "Settings has one scrolling body");
        self.body = Some(bounds);
        self.content = Some(content);
        self.offset = offset;
        if let Some(y) = self.scroll_to {
            state.scroll_to(scrollable::AbsoluteOffset {
                x: None,
                y: Some(y.max(0.0)),
            });
        }
    }

    fn custom(&mut self, _: Option<&Id>, bounds: Rectangle, state: &mut dyn std::any::Any) {
        let Some(control) = state.downcast_ref::<accessibility::Control<Message>>() else {
            return;
        };
        if matches!(
            control.node.role(),
            accessibility::Role::Status | accessibility::Role::Alert
        ) {
            self.live_regions
                .push((control.node.role(), control.node.live()));
        }
        let Some(label) = control.node.label() else {
            return;
        };
        if label.starts_with("Assistant ")
            || label.contains("assistant API key")
            || label == "Test assistant connection"
            || label == "Suggested assistant model"
        {
            self.controls.push(ControlBounds {
                label: label.into(),
                bounds,
                role: control.node.role(),
            });
        }
    }
}

fn footer(snapshot: &accesskit::TreeUpdate) -> accesskit::Rect {
    snapshot
        .nodes
        .iter()
        .find(|(_, node)| {
            node.label()
                .is_some_and(|label| label.starts_with("Preferences apply automatically."))
        })
        .unwrap_or_else(|| {
            let labels: Vec<_> = snapshot
                .nodes
                .iter()
                .filter_map(|(_, node)| node.label())
                .take(40)
                .collect();
            let texts: Vec<_> = snapshot
                .nodes
                .iter()
                .filter_map(|(_, node)| node.value())
                .take(40)
                .collect();
            panic!(
                "the actual Settings footer is exposed; {} nodes, labels: {labels:?}, values: {texts:?}",
                snapshot.nodes.len()
            )
        })
        .1
        .bounds()
        .expect("the footer has its actual text bounds")
}

fn render(
    name: &str,
    view: &mut Element<'_, Message>,
    tree: &mut Tree,
    node: &layout::Node,
    renderer: &mut iced::Renderer,
    size: Size,
    appearance: (f32, bool),
) {
    use iced::advanced::renderer::Renderer;
    let (scale, dark) = appearance;
    let Ok(directory) = std::env::var("CAPTUREFAB_LAYOUT_ARTIFACTS") else {
        return;
    };
    prepare_artifact_frame(view, tree, node, renderer, size);
    let viewport = Rectangle::new(Point::ORIGIN, size);
    renderer.reset(viewport);
    view.as_widget().draw(
        tree,
        renderer,
        if dark { &Theme::Dark } else { &Theme::Light },
        &iced::advanced::renderer::Style::default(),
        Layout::new(node),
        iced::mouse::Cursor::Unavailable,
        &viewport,
    );
    let physical = Size::new(
        (size.width * scale).round() as u32,
        (size.height * scale).round() as u32,
    );
    let pixels = renderer.screenshot(physical, scale, Palette::of(dark).base);
    std::fs::create_dir_all(&directory).unwrap();
    let file = std::fs::File::create(PathBuf::from(directory).join(format!("{name}.png"))).unwrap();
    let mut encoder = png::Encoder::new(file, physical.width, physical.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()
        .unwrap()
        .write_image_data(&pixels)
        .unwrap();
}

#[test]
fn compact_settings_reveal_complete_assistant_controls_in_both_presentations() {
    let mut renderer = renderer();
    for sheet in [false, true] {
        for scale in UiScale::ALL {
            for dark in [false, true] {
                for scene in [
                    "settings-assistant",
                    "settings-assistant-error",
                    "settings-assistant-success",
                ] {
                    let error = scene.ends_with("-error");
                    let success = scene.ends_with("-success");
                    let physical = if sheet {
                        Size::new(900.0, 620.0)
                    } else {
                        crate::gui::settings::window_settings().size
                    };
                    let viewport = physical / scale.factor();
                    let mut app = Workbench::new(
                        SessionHandle::new(),
                        "assistant-settings-layout".into(),
                        true,
                        None,
                    );
                    app.ui_scale = scale;
                    app.appearance = if dark {
                        Appearance::Dark
                    } else {
                        Appearance::Light
                    };
                    assert!(app.scene_assistant_settings(scene, false));
                    let mut view = if sheet {
                        app.settings_sheet(Palette::of(dark), (viewport.height - 80.0).max(320.0))
                    } else {
                        app.settings_window(Palette::of(dark))
                    };
                    let mut tree = Tree::new(view.as_widget());
                    let node = view.as_widget_mut().layout(
                        &mut tree,
                        &renderer,
                        &layout::Limits::new(Size::ZERO, viewport),
                    );
                    let (top, _, _) = accessibility::snapshot_widget(
                        view.as_widget_mut(),
                        &mut tree,
                        Layout::new(&node),
                        &renderer,
                    );
                    let pinned = footer(&top);
                    let mut geometry = SettingsGeometry::default();
                    view.as_widget_mut().operate(
                        &mut tree,
                        Layout::new(&node),
                        &renderer,
                        &mut geometry,
                    );
                    let body = geometry.body.unwrap();
                    assert!(geometry.content.unwrap().height > body.height);
                    assert!(
                        body.height >= 80.0 && body.y + body.height < pinned.y0 as f32,
                        "the form has usable space above its pinned footer"
                    );
                    assert!(
                        pinned.x0 >= -0.5
                            && pinned.y0 >= -0.5
                            && pinned.x1 <= f64::from(viewport.width) + 0.5
                            && pinned.y1 <= f64::from(viewport.height) + 0.5,
                        "the whole footer fits, sheet={sheet}, {scale}, {pinned:?}"
                    );
                    let prefix = format!(
                        "assistant-settings-{}-{scale}-{}-{scene}",
                        if sheet { "sheet" } else { "native" },
                        if dark { "dark" } else { "light" },
                    );
                    render(
                        &format!("{prefix}-top"),
                        &mut view,
                        &mut tree,
                        &node,
                        &mut renderer,
                        viewport,
                        (scale.factor(), dark),
                    );
                    assert!(
                        geometry.controls.len() >= 8,
                        "the full assistant form is present"
                    );
                    assert!(
                        geometry.live_regions.contains(&(
                            accessibility::Role::Status,
                            Some(accesskit::Live::Polite)
                        ))
                    );
                    if error {
                        assert!(geometry.live_regions.contains(&(
                            accessibility::Role::Alert,
                            Some(accesskit::Live::Assertive)
                        )));
                    }
                    assert!(geometry.controls.iter().any(|control| {
                        control.label == "Assistant provider"
                            && control.role == accessibility::Role::ComboBox
                    }));
                    assert!(geometry.controls.iter().any(|control| {
                        control.label == "Assistant model"
                            && control.role == accessibility::Role::TextInput
                    }));
                    assert!(geometry.controls.iter().any(|control| {
                        control.label.contains("assistant API key")
                            && control.role == accessibility::Role::PasswordInput
                    }));
                    assert_eq!(
                        geometry.controls.iter().any(|control| {
                            control.label == "Assistant setup complete: open Quick Command"
                                && control.role == accessibility::Role::Button
                        }),
                        success,
                        "only a successful test offers the continuation action"
                    );
                    for control in &geometry.controls {
                        // The operation sees unshifted layouts. Reveal each
                        // complete field or action without dispatching it.
                        let offset =
                            control.bounds.y - body.y - (body.height - control.bounds.height) / 2.0;
                        let mut scroll = SettingsGeometry {
                            scroll_to: Some(offset),
                            ..SettingsGeometry::default()
                        };
                        view.as_widget_mut().operate(
                            &mut tree,
                            Layout::new(&node),
                            &renderer,
                            &mut scroll,
                        );
                        let (snapshot, _, _) = accessibility::snapshot_widget(
                            view.as_widget_mut(),
                            &mut tree,
                            Layout::new(&node),
                            &renderer,
                        );
                        assert_eq!(footer(&snapshot), pinned);
                        let exposed = &snapshot
                            .nodes
                            .iter()
                            .find(|(_, node)| node.label() == Some(control.label.as_str()))
                            .unwrap_or_else(|| panic!("{} remains exposed", control.label))
                            .1;
                        let bounds = exposed.bounds().unwrap();
                        assert!(
                            (bounds.width() - f64::from(control.bounds.width)).abs() < 0.6
                                && (bounds.height() - f64::from(control.bounds.height)).abs() < 0.6
                                && bounds.x0 >= f64::from(body.x) - 0.5
                                && bounds.x1 <= f64::from(body.x + body.width) + 0.5
                                && bounds.y0 >= f64::from(body.y) - 0.5
                                && bounds.y1 <= f64::from(body.y + body.height) + 0.5
                                && bounds.y1 < pinned.y0,
                            "{} is entirely visible: sheet={sheet}, {scale}, error={error}, {bounds:?}; full {:?}",
                            control.label,
                            control.bounds,
                        );
                        if error
                            && matches!(
                                control.label.as_str(),
                                "Assistant model" | "Assistant base url"
                            )
                        {
                            assert_eq!(exposed.invalid(), Some(accesskit::Invalid::True));
                            assert!(exposed.description().is_some());
                        }
                        if control.role == accessibility::Role::PasswordInput {
                            assert!(
                                exposed
                                    .value()
                                    .is_none_or(|value| !value.contains("synthetic-key"))
                            );
                        }
                        if control.label == "Test assistant connection" {
                            render(
                                &format!("{prefix}-connection"),
                                &mut view,
                                &mut tree,
                                &node,
                                &mut renderer,
                                viewport,
                                (scale.factor(), dark),
                            );
                        }
                    }
                    assert_focus_traversal_reveals(&mut view, &mut tree, &node, &renderer);
                    let mut end = SettingsGeometry {
                        scroll_to: Some(f32::MAX),
                        ..SettingsGeometry::default()
                    };
                    view.as_widget_mut().operate(
                        &mut tree,
                        Layout::new(&node),
                        &renderer,
                        &mut end,
                    );
                    let mut bottom = SettingsGeometry::default();
                    view.as_widget_mut().operate(
                        &mut tree,
                        Layout::new(&node),
                        &renderer,
                        &mut bottom,
                    );
                    assert!(
                        (bottom.offset.y - (bottom.content.unwrap().height - body.height).round())
                            .abs()
                            < 0.6,
                        "the actual body reaches its bottom"
                    );
                    render(
                        &format!("{prefix}-bottom"),
                        &mut view,
                        &mut tree,
                        &node,
                        &mut renderer,
                        viewport,
                        (scale.factor(), dark),
                    );
                }
            }
        }
    }
}
