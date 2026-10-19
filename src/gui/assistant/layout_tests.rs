//! The actual quick command sheet: pinned controls, native semantics and
//! keyboard editing stay usable at compact sizes and every UI scale.
use super::*;
use crate::gui::{
    accessibility,
    destinations::layout_tests::{
        apply, assert_focus_traversal_reveals, prepare_artifact_frame, renderer,
    },
};
use iced::advanced::{
    Layout, layout,
    renderer::Headless,
    widget::{Id, Operation, Tree},
};
use iced::{Event, Point, Rectangle};
use serde_json::{Value, json};

fn view<'a>(state: &'a quick::State, p: &'static Palette, height: f32) -> Element<'a, Message> {
    accessibility::mapped(sheet::content(state, p, height, 0), |input| {
        Message::Assistant(Input::Quick(input))
    })
}

fn artifact(
    name: &str,
    view: &mut Element<'_, Message>,
    tree: &mut Tree,
    node: &layout::Node,
    renderer: &mut iced::Renderer,
    size: Size,
    appearance: (bool, f32),
) {
    use iced::advanced::renderer::Renderer;
    let Ok(directory) = std::env::var("CAPTUREFAB_LAYOUT_ARTIFACTS") else {
        return;
    };
    let (dark, scale) = appearance;
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
fn quick_command_scenes_keep_prompt_close_and_approval_controls_inside_the_sheet() {
    let mut renderer = renderer();
    for scene in scenes::WORDS {
        for dark in [false, true] {
            for scale in UiScale::ALL {
                for physical in [Size::new(1440.0, 900.0), Size::new(480.0, 620.0)] {
                    let size = physical / scale.factor();
                    let state = scenes::fixture(scene, SessionSnapshot::default());
                    let mut view = view(&state, Palette::of(dark), size.height);
                    let mut tree = Tree::new(view.as_widget());
                    let node = view.as_widget_mut().layout(
                        &mut tree,
                        &renderer,
                        &layout::Limits::new(Size::ZERO, size),
                    );
                    let (snapshot, _, _) = accessibility::snapshot_widget(
                        view.as_widget_mut(),
                        &mut tree,
                        Layout::new(&node),
                        &renderer,
                    );
                    assert!(snapshot.nodes.iter().any(|(_, node)| {
                        node.role() == accessibility::Role::Dialog
                            && node.label() == Some("Quick Command")
                    }));
                    for label in [
                        "Close Quick Command",
                        "Quick command request",
                        "Assistant provider and model",
                        "See camera images",
                    ] {
                        let control = snapshot
                            .nodes
                            .iter()
                            .find(|(_, node)| node.label() == Some(label))
                            .unwrap_or_else(|| panic!("{scene} exposes {label}"))
                            .1
                            .clone();
                        let bounds = control.bounds().unwrap();
                        assert!(
                            bounds.x0 >= -0.5
                                && bounds.y0 >= -0.5
                                && bounds.x1 <= f64::from(size.width) + 0.5
                                && bounds.y1 <= f64::from(size.height) + 0.5,
                            "{scene} {scale} {physical:?}: {label} is clipped: {bounds:?}",
                        );
                    }
                    if !state.plan.steps.is_empty() {
                        let label = state.run_label();
                        let run = snapshot
                            .nodes
                            .iter()
                            .find(|(_, node)| node.label() == Some(label.as_str()))
                            .unwrap()
                            .1
                            .clone();
                        let bounds = run.bounds().unwrap();
                        assert!(
                            bounds.x1 <= f64::from(size.width) + 0.5
                                && bounds.y1 <= f64::from(size.height) + 0.5,
                            "the approval button stays pinned and visible: {bounds:?}"
                        );
                        assert_eq!(run.is_disabled(), !state.ready_to_run());
                    }
                    if scene == "quick-command-setup" {
                        assert!(
                            snapshot
                                .nodes
                                .iter()
                                .any(|(_, node)| node.label()
                                    == Some("Set up an assistant provider"))
                        );
                    }
                    if matches!(scene, "quick-command-running" | "quick-command-thinking") {
                        let progress = snapshot
                            .nodes
                            .iter()
                            .find(|(_, node)| {
                                node.role() == accessibility::Role::Status
                                    && node.label().is_some_and(|label| {
                                        label.contains("Running") || label.contains("Asking")
                                    })
                            })
                            .expect("work progress has a live status region")
                            .1
                            .bounds()
                            .unwrap();
                        assert!(
                            progress.height() > 0.0
                                && progress.y0 >= 0.0
                                && progress.y1 <= f64::from(size.height) + 0.5,
                            "progress stays visible when the transcript is scrolled: {progress:?}"
                        );
                        assert!(
                            snapshot
                                .nodes
                                .iter()
                                .any(|(_, node)| node.label() == Some("Stop assistant work")
                                    && !node.is_disabled())
                        );
                    }
                    if scene == "quick-command-error" {
                        assert!(
                            snapshot
                                .nodes
                                .iter()
                                .any(|(_, node)| node.role() == accessibility::Role::Alert)
                        );
                    }
                    if scale == UiScale::Standard {
                        artifact(
                            &format!(
                                "{scene}-{}-{}x{}",
                                if dark { "dark" } else { "light" },
                                physical.width,
                                physical.height
                            ),
                            &mut view,
                            &mut tree,
                            &node,
                            &mut renderer,
                            size,
                            (dark, scale.factor()),
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn invalid_camera_and_plan_attention_have_native_semantics_without_repeated_alerts() {
    let renderer = renderer();
    let state = scenes::fixture("quick-command-invalid", SessionSnapshot::default());
    let mut view = view(&state, Palette::of(false), 800.0);
    let mut tree = Tree::new(view.as_widget());
    let node = view.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, Size::new(720.0, 800.0)),
    );
    let (snapshot, _, _) = accessibility::snapshot_widget(
        view.as_widget_mut(),
        &mut tree,
        Layout::new(&node),
        &renderer,
    );
    let stale = snapshot
        .nodes
        .iter()
        .find(|(_, node)| {
            node.role() == accessibility::Role::ComboBox
                && node
                    .value()
                    .is_some_and(|value| value.contains("(not available)"))
        })
        .expect("the unavailable camera remains visibly selected")
        .1
        .clone();
    assert_eq!(stale.invalid(), Some(accesskit::Invalid::True));
    assert!(snapshot.nodes.iter().any(|(_, node)| {
        node.role() == accessibility::Role::Status
            && node
                .label()
                .is_some_and(|label| label.contains("needs attention"))
    }));
    assert!(snapshot.nodes.iter().any(|(_, node)| {
        node.role() == accessibility::Role::Button
            && node
                .label()
                .is_some_and(|label| label.starts_with("Step 2 needs attention"))
    }));
    assert_eq!(
        snapshot
            .nodes
            .iter()
            .filter(|(_, node)| node.role() == accessibility::Role::Alert)
            .count(),
        0,
        "untouched issues use quiet captions, not a chorus of live alerts"
    );
}

#[derive(Default)]
struct TextFields(Vec<Id>);
impl Operation for TextFields {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        operate(self);
    }
    fn text_input(
        &mut self,
        id: Option<&Id>,
        _: Rectangle,
        _: &mut dyn iced::advanced::widget::operation::TextInput,
    ) {
        if let Some(id) = id {
            self.0.push(id.clone());
        }
    }
}

fn enter(
    view: &mut Element<'_, Message>,
    tree: &mut Tree,
    node: &layout::Node,
    renderer: &iced::Renderer,
) -> Vec<Message> {
    press_key(view, tree, node, renderer, keyboard::key::Named::Enter)
}

fn press_key(
    view: &mut Element<'_, Message>,
    tree: &mut Tree,
    node: &layout::Node,
    renderer: &iced::Renderer,
    named: keyboard::key::Named,
) -> Vec<Message> {
    let key = keyboard::Key::Named(named);
    let event = Event::Keyboard(keyboard::Event::KeyPressed {
        key: key.clone(),
        modified_key: key,
        physical_key: keyboard::key::Physical::Unidentified(
            keyboard::key::NativeCode::Unidentified,
        ),
        location: keyboard::Location::Standard,
        modifiers: keyboard::Modifiers::empty(),
        text: None,
        repeat: false,
    });
    let mut messages = Vec::new();
    view.as_widget_mut().update(
        tree,
        &event,
        Layout::new(node),
        iced::mouse::Cursor::Unavailable,
        renderer,
        &mut iced::advanced::clipboard::Null,
        &mut iced::advanced::Shell::new(&mut messages),
        &Rectangle::new(Point::ORIGIN, node.size()),
    );
    messages
}

#[derive(Default)]
struct BodyPosition {
    offset: f32,
    maximum: f32,
}
impl Operation for BodyPosition {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        operate(self);
    }
    fn scrollable(
        &mut self,
        id: Option<&Id>,
        bounds: Rectangle,
        content: Rectangle,
        translation: iced::Vector,
        _: &mut dyn iced::advanced::widget::operation::Scrollable,
    ) {
        if id == Some(&Id::new(crate::gui::widgets::SHEET_BODY)) {
            self.offset = translation.y;
            self.maximum = (content.height - bounds.height).max(0.0).round();
        }
    }
}

#[test]
fn bottom_anchored_quick_command_history_keeps_home_and_end_in_visual_order() {
    let renderer = renderer();
    let state = scenes::fixture("quick-command-plan", SessionSnapshot::default());
    let mut view = view(&state, Palette::of(false), 620.0);
    let mut tree = Tree::new(view.as_widget());
    let node = view.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, Size::new(480.0, 620.0)),
    );
    let position = |view: &mut Element<'_, Message>, tree: &mut Tree| {
        let mut position = BodyPosition::default();
        view.as_widget_mut()
            .operate(tree, Layout::new(&node), &renderer, &mut position);
        position
    };
    let initial = position(&mut view, &mut tree);
    assert!(initial.maximum > 100.0);
    assert!((initial.offset - initial.maximum).abs() <= 1.0);
    for (key, at_bottom) in [
        (keyboard::key::Named::Home, false),
        (keyboard::key::Named::End, true),
    ] {
        assert!(press_key(&mut view, &mut tree, &node, &renderer, key).is_empty());
        let current = position(&mut view, &mut tree);
        let expected = if at_bottom { current.maximum } else { 0.0 };
        assert!(
            (current.offset - expected).abs() <= 1.0,
            "{key:?}: {} versus {expected}",
            current.offset
        );
    }
}

#[test]
fn return_in_every_generic_text_editor_and_on_remove_never_runs_a_plan() {
    use crate::tools::{FieldKind, ToolCall};
    let renderer = renderer();
    let mut state = scenes::fixture("quick-command", SessionSnapshot::default());
    for spec in crate::tools::catalog() {
        let mut args = serde_json::Map::new();
        for field in spec.fields {
            if field.ui_only {
                continue;
            }
            let value = match field.kind {
                FieldKind::Camera => json!("sim:0"),
                FieldKind::Destination => Value::Null,
                FieldKind::Duration => json!("30s"),
                FieldKind::Time => json!("17:00"),
                FieldKind::Integer { .. } | FieldKind::Number { .. } => json!(1),
                FieldKind::Text => json!("Inspection"),
                FieldKind::FileName => json!("frame-{seq}.png"),
                FieldKind::Choice(values) => json!(values.first().map(|value| value.0)),
                FieldKind::Bool => json!(false),
                FieldKind::Url => json!("rtsp://camera.example/live"),
                FieldKind::Path => json!("/tmp/capturefab-scene"),
                FieldKind::Tags => json!({"line":"one"}),
                FieldKind::Features => json!([{"feature":"Gain","value":"0"}]),
                FieldKind::Region => json!([0, 0, 1, 1]),
                FieldKind::Secret => Value::Null,
            };
            args.insert(field.key.into(), value);
        }
        let id = state
            .plan
            .propose(
                ToolCall {
                    tool: spec.name.into(),
                    args: Value::Object(args),
                },
                None,
            )
            .unwrap();
        state.options_open.insert(id);
    }
    state.plan.recheck(
        state.context.as_ref().unwrap(),
        None,
        &crate::tools::EmptySecrets,
    );
    let mut view = view(&state, Palette::of(false), 900.0);
    let mut tree = Tree::new(view.as_widget());
    let node = view.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, Size::new(720.0, 900.0)),
    );
    let mut fields = TextFields::default();
    view.as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut fields);
    assert!(
        fields.0.len() > 20,
        "the real FieldSpec forms expose their text editors"
    );
    for id in fields.0 {
        apply(
            &mut view,
            &mut tree,
            &node,
            &renderer,
            iced::advanced::widget::operation::focusable::focus::<Message>(id),
        );
        assert!(
            enter(&mut view, &mut tree, &node, &renderer)
                .iter()
                .all(|message| !matches!(
                    message,
                    Message::Assistant(Input::Quick(quick::Input::Run(_)))
                ))
        );
    }
    for step in &state.plan.steps {
        apply(
            &mut view,
            &mut tree,
            &node,
            &renderer,
            iced::advanced::widget::operation::focusable::focus::<Message>(Id::from(format!(
                "step-{}-remove",
                step.id.0
            ))),
        );
        assert!(
            enter(&mut view, &mut tree, &node, &renderer)
                .iter()
                .all(|message| !matches!(
                    message,
                    Message::Assistant(Input::Quick(quick::Input::Run(_)))
                ))
        );
    }
}

#[test]
fn compact_plan_keeps_every_keyboard_control_reachable_in_both_directions() {
    let renderer = renderer();
    for dark in [false, true] {
        let state = scenes::fixture("quick-command-plan", SessionSnapshot::default());
        let mut view = view(&state, Palette::of(dark), 620.0);
        let mut tree = Tree::new(view.as_widget());
        let size = Size::new(480.0, 620.0);
        let node = view.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, size),
        );
        assert_focus_traversal_reveals(&mut view, &mut tree, &node, &renderer);
    }
}
