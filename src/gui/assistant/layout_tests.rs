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

#[test]
fn exported_plan_copy_uses_this_workbench_and_exact_path_with_a_dry_run() {
    use iced::futures::StreamExt;
    let renderer = renderer();
    for windows in [false, true] {
        for custom_directory in [false, true] {
            for physical in [Size::new(1100.0, 700.0), Size::new(480.0, 620.0)] {
                let session = "bench's $(name);";
                let mut app = Workbench::new(SessionHandle::new(), session.into(), true, None);
                app.assistant.quick = scenes::fixture("quick-command", SessionSnapshot::default());
                let (binary, directory, path, root) = if windows {
                    (
                        "C:\\Program Files\\Capturefab's\\capturefab.exe",
                        "C:\\Session's $files;",
                        "C:\\plan's $(file);.json",
                        "D:\\Run's $root;",
                    )
                } else {
                    (
                        "/Applications/Capturefab's App/capturefab",
                        "/tmp/session's $(directory);",
                        "/tmp/plan's $(file);.json",
                        "/Volumes/Run's $(root);",
                    )
                };
                app.assistant.commands = agents::CommandContext::fixture(
                    Some(binary),
                    custom_directory.then_some(directory),
                    windows,
                );
                app.assistant.quick.exported_plan = Some(PathBuf::from(path));
                app.assistant.quick.context.as_mut().unwrap().capture_root = PathBuf::from(root);
                let expected = app
                    .assistant
                    .commands
                    .plan_preview(
                        session,
                        std::path::Path::new(path),
                        std::path::Path::new(root),
                    )
                    .unwrap()
                    .command;
                let copied = {
                    let mut view = app.assistant_sheet(Palette::of(false), physical.height);
                    let mut tree = Tree::new(view.as_widget());
                    let node = view.as_widget_mut().layout(
                        &mut tree,
                        &renderer,
                        &layout::Limits::new(Size::ZERO, physical),
                    );
                    apply(
                        &mut view,
                        &mut tree,
                        &node,
                        &renderer,
                        iced::advanced::widget::operation::focusable::focus::<Message>(Id::new(
                            "quick-command-export-preview",
                        )),
                    );
                    apply(
                        &mut view,
                        &mut tree,
                        &node,
                        &renderer,
                        accessibility::focus_reveal_operation(app.main_window),
                    );
                    let (snapshot, bridge, window) = accessibility::snapshot_widget(
                        view.as_widget_mut(),
                        &mut tree,
                        Layout::new(&node),
                        &renderer,
                    );
                    let (id, copy) = snapshot
                        .nodes
                        .iter()
                        .find(|(_, node)| {
                            node.label() == Some("Copy exported plan preview command")
                        })
                        .unwrap();
                    let bounds = copy.bounds().unwrap();
                    assert!(
                        bounds.width() > 0.0
                            && bounds.height() > 0.0
                            && bounds.y0 >= 0.0
                            && bounds.y1 <= f64::from(physical.height) + 0.5
                    );
                    assert!(
                        snapshot
                            .nodes
                            .iter()
                            .any(|(_, node)| node.label() == Some(expected.as_str()))
                    );
                    assert!(
                        snapshot.nodes.iter().any(|(_, node)| node.label()
                            == Some(format!("Output folder: {root}").as_str()))
                    );
                    assert!(
                        snapshot.nodes.iter().any(|(_, node)| node
                            .label()
                            .is_some_and(|text| text
                                .contains("checks the plan without changing the session")
                                && text.contains("before removing --dry-run")))
                    );
                    let mut stream =
                        iced_runtime::task::into_stream(bridge.dispatch(accessibility::Request {
                            window,
                            replay: None,
                            action: accesskit::ActionRequest {
                                action: accesskit::Action::Click,
                                target: *id,
                                data: None,
                            },
                        }))
                        .unwrap();
                    let iced_runtime::Action::Output(Message::Assistant(Input::Quick(
                        quick::Input::Copy(command),
                    ))) = iced::futures::executor::block_on(stream.next()).unwrap()
                    else {
                        panic!("actual copy control emits the preview command")
                    };
                    command
                };
                assert_eq!(copied, expected);
                let task = app.update_assistant(Input::Quick(quick::Input::Copy(copied)));
                let mut stream = iced_runtime::task::into_stream(task).unwrap();
                let iced_runtime::Action::Clipboard(iced_runtime::clipboard::Action::Write {
                    contents,
                    ..
                }) = iced::futures::executor::block_on(stream.next()).unwrap()
                else {
                    panic!("copy writes to the clipboard")
                };
                assert_eq!(
                    contents, expected,
                    "clipboard preserves quoted locators exactly"
                );
                assert!(app.pending.is_empty());
                assert!(app.handle.snapshot().signals.is_empty());
            }
        }
    }
}

#[test]
fn exported_plan_does_not_offer_a_path_dependent_command_without_executable_or_reviewed_root() {
    let renderer = renderer();
    for missing_context in [false, true] {
        let mut app = Workbench::new(SessionHandle::new(), "bench".into(), true, None);
        app.assistant.commands = agents::CommandContext::fixture(
            missing_context.then_some("/opt/capturefab"),
            None,
            false,
        );
        app.assistant.quick = scenes::fixture("quick-command", SessionSnapshot::default());
        if missing_context {
            app.assistant.quick.context = None;
        }
        app.assistant.quick.exported_plan = Some(PathBuf::from("/tmp/plan.json"));
        let mut view = app.assistant_sheet(Palette::of(false), 700.0);
        let mut tree = Tree::new(view.as_widget());
        let node = view.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(700.0, 700.0)),
        );
        let (snapshot, _, _) = accessibility::snapshot_widget(
            view.as_widget_mut(),
            &mut tree,
            Layout::new(&node),
            &renderer,
        );
        assert!(
            !snapshot
                .nodes
                .iter()
                .any(|(_, node)| node.label() == Some("Copy exported plan preview command"))
        );
        assert!(snapshot.nodes.iter().any(|(_, node)| {
            node.label().is_some_and(|text| {
                text.contains("preview command is unavailable") && text.contains("/tmp/plan.json")
            })
        }));
    }
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
                        "Read sensor values and dataset rows",
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
    bounds: Option<Rectangle>,
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
            self.bounds = Some(bounds);
        }
    }
}

#[test]
fn empty_quick_command_starts_with_its_heading_and_suggestions_above_consent() {
    let mut renderer = renderer();
    for scene in ["quick-command", "quick-command-data-consent"] {
        for dark in [false, true] {
            for scale in UiScale::ALL {
                let size = Size::new(480.0, 620.0) / scale.factor();
                let state = scenes::fixture(scene, SessionSnapshot::default());
                let mut view = view(&state, Palette::of(dark), size.height);
                let mut tree = Tree::new(view.as_widget());
                let node = view.as_widget_mut().layout(
                    &mut tree,
                    &renderer,
                    &layout::Limits::new(Size::ZERO, size),
                );
                let mut position = BodyPosition::default();
                view.as_widget_mut().operate(
                    &mut tree,
                    Layout::new(&node),
                    &renderer,
                    &mut position,
                );
                assert!(
                    position.offset.abs() <= 1.0,
                    "{scene} {scale} starts at the top"
                );
                let body = position.bounds.unwrap();
                let (snapshot, _, _) = accessibility::snapshot_widget(
                    view.as_widget_mut(),
                    &mut tree,
                    Layout::new(&node),
                    &renderer,
                );
                for label in [
                    "What would you like to capture?",
                    "Suggested request: Take a photo",
                ] {
                    let control = &snapshot
                        .nodes
                        .iter()
                        .find(|(_, node)| node.label() == Some(label))
                        .unwrap()
                        .1;
                    let bounds = control.bounds().unwrap();
                    assert!(
                        bounds.width() > 0.0
                            && bounds.height() > 0.0
                            && bounds.y0 >= f64::from(body.y) - 0.5
                            && bounds.y1 <= f64::from(body.y + body.height) + 0.5,
                        "{scene} {scale}: {label} at {bounds:?} must be visible in {body:?}"
                    );
                }
                if scale == UiScale::Standard {
                    artifact(
                        &format!(
                            "{scene}-initial-{}-480x620",
                            if dark { "dark" } else { "light" }
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

#[test]
fn quick_source_binding_controls_are_local_and_reachable_at_compact_scales() {
    let mut renderer = renderer();
    for dark in [false, true] {
        for scale in UiScale::ALL {
            let size = Size::new(480.0, 620.0) / scale.factor();
            let state = scenes::fixture("quick-command-data-sources", SessionSnapshot::default());
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
            for label in [
                "Quick command data source endpoint",
                "Add data source to Quick Command",
            ] {
                let input = &snapshot
                    .nodes
                    .iter()
                    .find(|(_, node)| node.label() == Some(label))
                    .unwrap()
                    .1;
                assert!(
                    !input.is_disabled(),
                    "{label} supports deliberate local editing"
                );
            }
            let source = &state.data_sources[0];
            assert!(snapshot.nodes.iter().any(|(_, node)| node.label()
                == Some(format!("Remove approved source {}", source.reference).as_str())));
            assert!(
                snapshot
                    .nodes
                    .iter()
                    .any(|(_, node)| node.role() == accessibility::Role::CheckBox
                        && node.label() == Some("Read sensor values and dataset rows")
                        && node.toggled() == Some(accesskit::Toggled::True))
            );
            assert_focus_traversal_reveals(&mut view, &mut tree, &node, &renderer);
            if scale == UiScale::Standard {
                apply(
                    &mut view,
                    &mut tree,
                    &node,
                    &renderer,
                    iced::advanced::widget::operation::focusable::focus::<Message>(Id::new(
                        "quick-command-data-source",
                    )),
                );
                apply(
                    &mut view,
                    &mut tree,
                    &node,
                    &renderer,
                    accessibility::focus_reveal_operation(iced::window::Id::unique()),
                );
                artifact(
                    &format!(
                        "quick-command-data-sources-{}-480x620",
                        if dark { "dark" } else { "light" }
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

#[test]
fn protected_quick_source_editor_masks_password_and_reveals_every_local_control() {
    let mut renderer = renderer();
    for dark in [false, true] {
        for scale in UiScale::ALL {
            for physical in [Size::new(480.0, 620.0), Size::new(1100.0, 700.0)] {
                let size = physical / scale.factor();
                let state = scenes::fixture("quick-command-data-auth", SessionSnapshot::default());
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
                for label in [
                    "Source profile name",
                    "Source endpoint",
                    "Source user name",
                    "Source authentication",
                    "Source password",
                    "Save protected source profile",
                ] {
                    let role = match label {
                        "Source authentication" => accessibility::Role::ComboBox,
                        "Source password" => accessibility::Role::PasswordInput,
                        "Save protected source profile" => accessibility::Role::Button,
                        _ => accessibility::Role::TextInput,
                    };
                    let control = &snapshot
                        .nodes
                        .iter()
                        .find(|(_, node)| node.label() == Some(label) && node.role() == role)
                        .unwrap_or_else(|| panic!("protected source editor exposes {label}"))
                        .1;
                    assert!(
                        !control.is_disabled(),
                        "{label} is editable before an assistant request"
                    );
                    if label == "Source password" {
                        assert_eq!(control.role(), accessibility::Role::PasswordInput);
                        assert!(
                            control
                                .value()
                                .is_none_or(|value| !value.contains("fixture-only-password"))
                        );
                    }
                }
                assert!(
                    snapshot
                        .nodes
                        .iter()
                        .any(|(_, node)| node.label() == Some("Send quick command")
                            && node.is_disabled())
                );
                assert_focus_traversal_reveals(&mut view, &mut tree, &node, &renderer);
                apply(
                    &mut view,
                    &mut tree,
                    &node,
                    &renderer,
                    iced::advanced::widget::operation::focusable::focus::<Message>(Id::new(
                        "quick-source-profile-save",
                    )),
                );
                apply(
                    &mut view,
                    &mut tree,
                    &node,
                    &renderer,
                    accessibility::focus_reveal_operation(iced::window::Id::unique()),
                );
                apply(
                    &mut view,
                    &mut tree,
                    &node,
                    &renderer,
                    iced::advanced::widget::operation::focusable::focus::<Message>(Id::new(
                        "quick-source-profile-password",
                    )),
                );
                let (snapshot, _, _) = accessibility::snapshot_widget(
                    view.as_widget_mut(),
                    &mut tree,
                    Layout::new(&node),
                    &renderer,
                );
                let password = &snapshot
                    .nodes
                    .iter()
                    .find(|(_, node)| {
                        node.label() == Some("Source password")
                            && node.role() == accessibility::Role::PasswordInput
                    })
                    .unwrap()
                    .1;
                let bounds = password.bounds().unwrap();
                assert!(
                    bounds.x0 >= -0.5
                        && bounds.y0 >= -0.5
                        && bounds.x1 <= f64::from(size.width) + 0.5
                        && bounds.y1 <= f64::from(size.height) + 0.5,
                    "protected password is visible after keyboard focus at {scale}: {bounds:?}"
                );
                if scale == UiScale::Standard {
                    artifact(
                        &format!(
                            "quick-command-data-auth-{}-{}x{}",
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
                FieldKind::StringList => json!(["temperature"]),
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
fn string_list_rows_have_single_name_editors_and_keyboard_add_remove_controls() {
    let mut renderer = renderer();
    for (tool, key, label) in [
        ("start_signal_capture", "tags", "Tags"),
        ("export_dataset", "tables", "Tables"),
        ("get_camera_features", "names", "Features"),
    ] {
        let mut state = scenes::fixture("quick-command", SessionSnapshot::default());
        let spec = crate::tools::spec(tool).unwrap();
        let mut args: serde_json::Map<String, Value> = (spec.schema)()["properties"]
            .as_object()
            .unwrap()
            .keys()
            .map(|key| (key.clone(), Value::Null))
            .collect();
        args.insert(key.into(), json!(["first", "second"]));
        let id = state
            .plan
            .propose(
                crate::tools::ToolCall {
                    tool: tool.into(),
                    args: Value::Object(args),
                },
                None,
            )
            .unwrap();
        state.options_open.insert(id);
        for dark in [false, true] {
            for scale in UiScale::ALL {
                let size = Size::new(480.0, 620.0) / scale.factor();
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
                for (index, value) in ["first", "second"].into_iter().enumerate() {
                    let input = snapshot
                        .nodes
                        .iter()
                        .find(|(_, node)| {
                            node.role() == accessibility::Role::TextInput
                                && node.label()
                                    == Some(format!("Step {} {label} {}", id.0, index + 1).as_str())
                        })
                        .expect("each name has one native text input");
                    assert_eq!(input.1.value(), Some(value));
                }
                assert!(!snapshot.nodes.iter().any(|(_, node)| {
                    node.label()
                        .is_some_and(|label| label.contains("Expected value"))
                }));
                assert_focus_traversal_reveals(&mut view, &mut tree, &node, &renderer);
                if scale == UiScale::Standard {
                    apply(
                        &mut view,
                        &mut tree,
                        &node,
                        &renderer,
                        iced::advanced::widget::operation::focusable::focus::<Message>(Id::from(
                            cards::field_id(id, &format!("{key}.0")),
                        )),
                    );
                    apply(
                        &mut view,
                        &mut tree,
                        &node,
                        &renderer,
                        accessibility::focus_reveal_operation(iced::window::Id::unique()),
                    );
                    artifact(
                        &format!(
                            "string-list-{key}-{}-480x620",
                            if dark { "dark" } else { "light" }
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
        let press = |state: &quick::State, control: String| {
            let mut view = view(state, Palette::of(false), 620.0);
            let mut tree = Tree::new(view.as_widget());
            let node = view.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, Size::new(480.0, 620.0)),
            );
            apply(
                &mut view,
                &mut tree,
                &node,
                &renderer,
                iced::advanced::widget::operation::focusable::focus::<Message>(Id::from(control)),
            );
            enter(&mut view, &mut tree, &node, &renderer)
        };
        let messages = press(&state, cards::field_id(id, &format!("{key}-add")));
        assert!(messages.iter().any(|message| matches!(
            message,
            Message::Assistant(Input::Quick(quick::Input::StringList { remove: None, .. }))
        )));
        for message in messages {
            if let Message::Assistant(Input::Quick(input)) = message {
                let _ = state.input(input, Instant::now());
            }
        }
        assert_eq!(
            state.plan.steps[0].call.args[key],
            json!(["first", "second", ""])
        );
        let messages = press(&state, cards::field_id(id, &format!("{key}.0-remove")));
        assert!(messages.iter().any(|message| matches!(
            message,
            Message::Assistant(Input::Quick(quick::Input::StringList {
                remove: Some(0),
                ..
            }))
        )));
        for message in messages {
            if let Message::Assistant(Input::Quick(input)) = message {
                let _ = state.input(input, Instant::now());
            }
        }
        assert_eq!(state.plan.steps[0].call.args[key], json!(["second", ""]));
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
