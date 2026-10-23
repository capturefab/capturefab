//! Task modes keep their stop control visible, approvals deliberate and every
//! editor keyboard reachable at all supported scales.
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

fn view<'a>(state: &'a tasks::State, p: &'static Palette, height: f32) -> Element<'a, Message> {
    accessibility::mapped(task_view::content(state, p, height, 0), |input| {
        Message::Assistant(Input::Task(input))
    })
}

fn snapshot(
    view: &mut Element<'_, Message>,
    tree: &mut Tree,
    node: &layout::Node,
    renderer: &iced::Renderer,
) -> accesskit::TreeUpdate {
    accessibility::snapshot_widget(view.as_widget_mut(), tree, Layout::new(node), renderer).0
}

fn control(snapshot: &accesskit::TreeUpdate, label: &str) -> accesskit::Node {
    snapshot
        .nodes
        .iter()
        .find(|(_, node)| node.label() == Some(label))
        .unwrap_or_else(|| panic!("the actual task sheet exposes {label}"))
        .1
        .clone()
}

fn inside(node: &accesskit::Node, size: Size, scene: &str) {
    let bounds = node.bounds().unwrap();
    assert!(
        bounds.width() > 0.0
            && bounds.height() > 0.0
            && bounds.x0 >= -0.5
            && bounds.y0 >= -0.5
            && bounds.x1 <= f64::from(size.width) + 0.5
            && bounds.y1 <= f64::from(size.height) + 0.5,
        "{scene}: {:?} is clipped at {bounds:?} in {size:?}",
        node.label(),
    );
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
fn task_scenes_keep_modes_status_and_stop_visible_at_every_scale() {
    let mut renderer = renderer();
    for scene in task_scenes::WORDS {
        for dark in [false, true] {
            for scale in UiScale::ALL {
                for physical in [Size::new(1440.0, 900.0), Size::new(480.0, 620.0)] {
                    let size = physical / scale.factor();
                    let state = task_scenes::fixture(scene, SessionSnapshot::default());
                    let mut view = view(&state, Palette::of(dark), size.height);
                    let mut tree = Tree::new(view.as_widget());
                    let node = view.as_widget_mut().layout(
                        &mut tree,
                        &renderer,
                        &layout::Limits::new(Size::ZERO, size),
                    );
                    let snapshot = snapshot(&mut view, &mut tree, &node, &renderer);
                    assert_eq!(
                        control(&snapshot, "Quick Command").role(),
                        accessibility::Role::Dialog,
                    );
                    for label in [
                        "Close Quick Command",
                        "How the assistant works",
                        "Assistant task provider and model",
                    ] {
                        inside(&control(&snapshot, label), size, scene);
                    }
                    if state.active() {
                        for label in ["Stop assistant task", "Take over camera control"] {
                            let control = control(&snapshot, label);
                            inside(&control, size, scene);
                            assert!(!control.is_disabled(), "{scene}: {label}");
                        }
                        let pause = if scene.ends_with("-paused-human") {
                            "Resume assistant task"
                        } else {
                            "Pause assistant task"
                        };
                        inside(&control(&snapshot, pause), size, scene);
                    } else if state.outcome.is_some() {
                        for label in [
                            "Review assistant setting restoration",
                            "Export assistant task log",
                            "New assistant task",
                        ] {
                            inside(&control(&snapshot, label), size, scene);
                        }
                    } else {
                        inside(&control(&snapshot, "Start assistant task"), size, scene);
                    }
                    if !state.snapshot.run_id.is_empty() {
                        let status = snapshot
                            .nodes
                            .iter()
                            .find(|(_, node)| {
                                node.role() == accessibility::Role::Status
                                    && node
                                        .label()
                                        .is_some_and(|label| label.starts_with("Assistant task:"))
                            })
                            .expect("a run has a live task status");
                        inside(&status.1, size, scene);
                        assert!(snapshot.nodes.iter().any(|(_, node)| {
                            node.role() == accessibility::Role::ProgressIndicator
                                && node
                                    .label()
                                    .is_some_and(|label| label.starts_with("Rounds "))
                        }));
                        assert!(snapshot.nodes.iter().any(|(_, node)| {
                            node.role() == accessibility::Role::Image
                                && node.label().is_some_and(|label| {
                                    label.contains("exact image sent to the assistant")
                                })
                        }));
                    }
                    if scene.ends_with("-approval") {
                        assert_eq!(
                            snapshot
                                .nodes
                                .iter()
                                .filter(|(_, node)| { node.role() == accessibility::Role::Alert })
                                .count(),
                            1,
                            "one checked approval has one alert"
                        );
                        inside(&control(&snapshot, "Approve assistant action"), size, scene);
                        inside(&control(&snapshot, "Deny assistant action"), size, scene);
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
fn compact_task_scope_and_approval_are_reachable_in_both_keyboard_directions() {
    let renderer = renderer();
    for scene in ["quick-command-task-setup", "quick-command-task-approval"] {
        for dark in [false, true] {
            let state = task_scenes::fixture(scene, SessionSnapshot::default());
            let mut view = view(&state, Palette::of(dark), 620.0);
            let mut tree = Tree::new(view.as_widget());
            let node = view.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, Size::new(480.0, 620.0)),
            );
            assert_focus_traversal_reveals(&mut view, &mut tree, &node, &renderer);
        }
    }
}

fn key(
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
struct Fields(Vec<Id>);
impl Operation for Fields {
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

#[test]
fn return_in_goal_limits_notes_and_buttons_never_starts_approves_or_stops() {
    let renderer = renderer();
    for scene in ["quick-command-task-setup", "quick-command-task-approval"] {
        let mut state = task_scenes::fixture(scene, SessionSnapshot::default());
        state.limits_open = true;
        let mut view = view(&state, Palette::of(false), 900.0);
        let mut tree = Tree::new(view.as_widget());
        let node = view.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(720.0, 900.0)),
        );
        let mut fields = Fields::default();
        view.as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut fields);
        fields.0.extend([
            Id::new("assistant-task-start"),
            Id::new("assistant-task-stop"),
            Id::new("assistant-approval-approve"),
        ]);
        for id in fields.0 {
            apply(
                &mut view,
                &mut tree,
                &node,
                &renderer,
                iced::advanced::widget::operation::focusable::focus::<Message>(id),
            );
            let messages = key(
                &mut view,
                &mut tree,
                &node,
                &renderer,
                keyboard::key::Named::Enter,
            );
            assert!(
                messages.iter().all(|message| !matches!(
                    message,
                    Message::Assistant(Input::Task(
                        tasks::Input::Start | tasks::Input::Stop | tasks::Input::Approve { .. }
                    ))
                )),
                "Return cannot authorize a camera action: {messages:?}"
            );
        }
    }
}

#[test]
fn approval_expiry_disables_authorization_without_hiding_checked_facts() {
    let renderer = renderer();
    let mut state = task_scenes::fixture("quick-command-task-approval", SessionSnapshot::default());
    state.now_ms = state.approval.as_ref().unwrap().expires_at_ms;
    let mut view = view(&state, Palette::of(false), 900.0);
    let mut tree = Tree::new(view.as_widget());
    let node = view.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, Size::new(720.0, 900.0)),
    );
    let snapshot = snapshot(&mut view, &mut tree, &node, &renderer);
    assert!(control(&snapshot, "Approve assistant action").is_disabled());
    assert!(!control(&snapshot, "Deny assistant action").is_disabled());
    let alert = snapshot
        .nodes
        .iter()
        .find(|(_, node)| node.role() == accessibility::Role::Alert)
        .unwrap();
    assert!(
        alert
            .1
            .label()
            .unwrap()
            .contains(&state.approval.as_ref().unwrap().summary)
    );
    assert!(
        state
            .approval
            .as_ref()
            .unwrap()
            .facts
            .iter()
            .any(|(name, value)| name.contains("ExposureTime") || value.contains("ExposureTime"))
    );
}

#[test]
fn completed_tasks_offer_reviewed_per_setting_restore_and_cleanup() {
    let renderer = renderer();
    let mut state = task_scenes::fixture("quick-command-task-done", SessionSnapshot::default());
    state.tab = tasks::Tab::Changes;
    let camera = &state.changes[0].camera;
    let mut view = view(&state, Palette::of(false), 900.0);
    let mut tree = Tree::new(view.as_widget());
    let node = view.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, Size::new(720.0, 900.0)),
    );
    let snapshot = snapshot(&mut view, &mut tree, &node, &renderer);
    for label in [
        format!("Restore ExposureTime on {camera}"),
        format!("Stop recording on {camera}"),
        format!("Cancel schedule on {camera}"),
        format!("Turn off auto mode on {camera}"),
    ] {
        assert!(!control(&snapshot, &label).is_disabled());
    }
}

#[test]
fn budget_nodes_expose_numeric_progress_and_zero_allowance_without_live_updates() {
    let renderer = renderer();
    for images in [0, 14] {
        let mut state =
            task_scenes::fixture("quick-command-task-running", SessionSnapshot::default());
        state.limits.max_images = images;
        let mut view = view(&state, Palette::of(false), 900.0);
        let mut tree = Tree::new(view.as_widget());
        let node = view.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(720.0, 900.0)),
        );
        let snapshot = snapshot(&mut view, &mut tree, &node, &renderer);
        let rounds = control(&snapshot, "Rounds 12 of 40");
        assert_eq!(rounds.value(), Some("Rounds 12 of 40"));
        assert_eq!(rounds.numeric_value(), Some(12.0));
        assert_eq!(rounds.min_numeric_value(), Some(0.0));
        assert_eq!(rounds.max_numeric_value(), Some(40.0));
        assert_eq!(rounds.live(), None);
        let images = control(&snapshot, &format!("Images 14 of {images}"));
        if state.limits.max_images == 0 {
            assert!(images.numeric_value().is_none());
            assert!(images.max_numeric_value().is_none());
        } else {
            assert_eq!(images.numeric_value(), Some(14.0));
            assert_eq!(images.max_numeric_value(), Some(14.0));
        }
    }
}

#[test]
fn unfinished_task_notice_has_a_log_action_and_does_not_render_as_running() {
    let renderer = renderer();
    let mut state = task_scenes::fixture("quick-command-task-setup", SessionSnapshot::default());
    state.crash = Some(tasks::CrashRow {
        run_id: "r-20261018-143200-abcd".into(),
        time_ms: state.now_ms - 60_000,
    });
    let mut view = view(&state, Palette::of(false), 900.0);
    let mut tree = Tree::new(view.as_widget());
    let node = view.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, Size::new(720.0, 900.0)),
    );
    let snapshot = snapshot(&mut view, &mut tree, &node, &renderer);
    assert!(!control(&snapshot, "View unfinished assistant task log").is_disabled());
    assert!(
        !snapshot
            .nodes
            .iter()
            .any(|(_, node)| node.label() == Some("Stop assistant task"))
    );
    assert!(!state.active());
}

#[test]
fn a_removed_selected_destination_stays_visible_and_can_be_unchecked() {
    let renderer = renderer();
    let mut state = task_scenes::fixture("quick-command-task-setup", SessionSnapshot::default());
    state.destinations.insert("removed-archive".into());
    assert!(state.start_reason().unwrap().contains("destination"));
    let messages = {
        let mut view = view(&state, Palette::of(false), 1800.0);
        let mut tree = Tree::new(view.as_widget());
        let node = view.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(720.0, 1800.0)),
        );
        let snapshot = snapshot(&mut view, &mut tree, &node, &renderer);
        let destination = control(&snapshot, "removed-archive (not available)");
        assert_eq!(destination.role(), accessibility::Role::CheckBox);
        assert_eq!(destination.toggled(), Some(accesskit::Toggled::True));
        let bounds = destination.bounds().unwrap();
        assert!(bounds.height() > 0.0);
        let cursor = iced::mouse::Cursor::Available(Point::new(
            ((bounds.x0 + bounds.x1) / 2.0) as f32,
            ((bounds.y0 + bounds.y1) / 2.0) as f32,
        ));
        let mut messages = Vec::new();
        for event in [
            iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left),
            iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left),
        ] {
            view.as_widget_mut().update(
                &mut tree,
                &Event::Mouse(event),
                Layout::new(&node),
                cursor,
                &renderer,
                &mut iced::advanced::clipboard::Null,
                &mut iced::advanced::Shell::new(&mut messages),
                &Rectangle::new(Point::ORIGIN, node.size()),
            );
        }
        messages
    };
    assert!(messages.iter().any(|message| matches!(message,
        Message::Assistant(Input::Task(tasks::Input::Destination(name, false))) if name == "removed-archive")));
    state.input(
        tasks::Input::Destination("removed-archive".into(), false),
        Instant::now(),
    );
    assert!(!state.destinations.contains("removed-archive"));
    assert!(
        !state
            .start_reason()
            .is_some_and(|reason| reason.contains("destination"))
    );
}

fn activate(
    view: &mut Element<'_, Message>,
    tree: &mut Tree,
    node: &layout::Node,
    renderer: &iced::Renderer,
    label: &str,
) -> Vec<Message> {
    let snapshot = snapshot(view, tree, node, renderer);
    let button = control(&snapshot, label);
    let bounds = button.bounds().unwrap();
    assert!(bounds.height() > 0.0 && bounds.width() > 0.0);
    let cursor = iced::mouse::Cursor::Available(Point::new(
        ((bounds.x0 + bounds.x1) / 2.0) as f32,
        ((bounds.y0 + bounds.y1) / 2.0) as f32,
    ));
    let mut messages = Vec::new();
    for event in [
        iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left),
        iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left),
    ] {
        view.as_widget_mut().update(
            tree,
            &Event::Mouse(event),
            Layout::new(node),
            cursor,
            renderer,
            &mut iced::advanced::clipboard::Null,
            &mut iced::advanced::Shell::new(&mut messages),
            &Rectangle::new(Point::ORIGIN, node.size()),
        );
    }
    messages
}

#[test]
fn fresh_plan_first_shows_unfinished_task_and_routes_log_controls_through_the_real_sheet() {
    let renderer = renderer();
    let mut app = Workbench::new(
        SessionHandle::new(),
        "task-plan-log-tests".into(),
        true,
        None,
    );
    app.assistant.quick = scenes::fixture("quick-command", SessionSnapshot::default());
    app.assistant.tasks.synthetic = true;
    app.assistant.tasks.crash = Some(tasks::CrashRow {
        run_id: "r-20261018-143200-abcd".into(),
        time_ms: crate::tools::tests::NOW,
    });
    assert_eq!(app.assistant.tasks.mode, tasks::Mode::Plan);
    let messages = {
        let mut view = app.assistant_sheet(Palette::of(false), 1800.0);
        let mut tree = Tree::new(view.as_widget());
        let node = view.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(720.0, 1800.0)),
        );
        let snapshot = snapshot(&mut view, &mut tree, &node, &renderer);
        assert!(!control(&snapshot, "View unfinished assistant task log").is_disabled());
        activate(
            &mut view,
            &mut tree,
            &node,
            &renderer,
            "View unfinished assistant task log",
        )
    };
    assert!(messages.iter().any(|message| matches!(
        message,
        Message::Assistant(Input::Quick(quick::Input::Task(tasks::Input::ViewLog)))
    )));
    app.assistant.tasks.log_text =
        Some("{\"kind\":\"start\",\"goal\":\"Inspect a camera\"}\n".into());
    let messages = {
        let mut view = app.assistant_sheet(Palette::of(false), 1800.0);
        let mut tree = Tree::new(view.as_widget());
        let node = view.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(720.0, 1800.0)),
        );
        activate(
            &mut view,
            &mut tree,
            &node,
            &renderer,
            "Close assistant task log",
        )
    };
    assert!(messages.iter().any(|message| matches!(
        message,
        Message::Assistant(Input::Quick(quick::Input::Task(tasks::Input::CloseLog)))
    )));
    let _ = app.update_assistant(Input::Quick(quick::Input::Task(tasks::Input::CloseLog)));
    assert!(app.assistant.tasks.log_text.is_none());
    assert_eq!(app.assistant.tasks.mode, tasks::Mode::Plan);
    assert!(!app.assistant.tasks.active());
}

#[test]
fn quit_guard_is_an_alert_dialog_with_deliberate_buttons_at_every_scale() {
    let renderer = renderer();
    for scale in UiScale::ALL {
        for physical in [Size::new(1440.0, 900.0), Size::new(480.0, 620.0)] {
            let mut app = Workbench::new(
                SessionHandle::new(),
                "task-quit-dialog-tests".into(),
                true,
                None,
            );
            assert!(app.scene_assistant_task("quick-command-task-running", false));
            let _ = app.guard_assistant_quit(CloseTarget::Quit);
            let size = physical / scale.factor();
            let mut view =
                app.assistant_quit_view(space().width(Fill).height(Fill).into(), app.main_window);
            let mut tree = Tree::new(view.as_widget());
            let node = view.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, size),
            );
            let snapshot = snapshot(&mut view, &mut tree, &node, &renderer);
            assert!(
                snapshot
                    .nodes
                    .iter()
                    .any(|(_, node)| node.role() == accessibility::Role::AlertDialog
                        && node.label() == Some("Stop the assistant and quit?"))
            );
            for label in ["Keep running", "Stop and quit"] {
                let control = control(&snapshot, label);
                inside(&control, size, "quit confirmation");
                assert!(!control.is_disabled());
            }
            apply(
                &mut view,
                &mut tree,
                &node,
                &renderer,
                iced::advanced::widget::operation::focusable::focus::<Message>(Id::new(
                    "assistant-stop-quit",
                )),
            );
            let messages = key(
                &mut view,
                &mut tree,
                &node,
                &renderer,
                keyboard::key::Named::Enter,
            );
            assert!(!messages.iter().any(|message| matches!(
                message,
                Message::Assistant(Input::Task(tasks::Input::ConfirmQuit | tasks::Input::Stop))
            )));
            let messages = activate(&mut view, &mut tree, &node, &renderer, "Keep running");
            assert!(messages.iter().any(|message| matches!(
                message,
                Message::Assistant(Input::Task(tasks::Input::KeepRunning))
            )));
        }
    }
}

fn release_space(
    view: &mut Element<'_, Message>,
    tree: &mut Tree,
    node: &layout::Node,
    renderer: &iced::Renderer,
) -> Vec<Message> {
    let mut messages = key(view, tree, node, renderer, keyboard::key::Named::Space);
    assert!(messages.is_empty(), "Space activates only on release");
    let event = Event::Keyboard(keyboard::Event::KeyReleased {
        key: keyboard::Key::Named(keyboard::key::Named::Space),
        modified_key: keyboard::Key::Named(keyboard::key::Named::Space),
        physical_key: keyboard::key::Physical::Unidentified(
            keyboard::key::NativeCode::Unidentified,
        ),
        location: keyboard::Location::Standard,
        modifiers: keyboard::Modifiers::empty(),
    });
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

#[test]
fn explicit_space_and_pointer_actions_still_start_approve_and_stop() {
    let renderer = renderer();
    for (index, scene, id, label) in [
        (
            0,
            "quick-command-task-setup",
            "assistant-task-start",
            "Start assistant task",
        ),
        (
            1,
            "quick-command-task-approval",
            "assistant-approval-approve",
            "Approve assistant action",
        ),
        (
            2,
            "quick-command-task-running",
            "assistant-task-stop",
            "Stop assistant task",
        ),
    ] {
        let state = task_scenes::fixture(scene, SessionSnapshot::default());
        let mut view = view(&state, Palette::of(false), 900.0);
        let mut tree = Tree::new(view.as_widget());
        let node = view.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(720.0, 900.0)),
        );
        apply(
            &mut view,
            &mut tree,
            &node,
            &renderer,
            iced::advanced::widget::operation::focusable::focus::<Message>(Id::from(id.to_owned())),
        );
        assert!(
            key(
                &mut view,
                &mut tree,
                &node,
                &renderer,
                keyboard::key::Named::Enter
            )
            .is_empty()
        );
        let intended = |message: &Message| {
            matches!(
                (index, message),
                (0, Message::Assistant(Input::Task(tasks::Input::Start)))
                    | (
                        1,
                        Message::Assistant(Input::Task(tasks::Input::Approve { .. }))
                    )
                    | (2, Message::Assistant(Input::Task(tasks::Input::Stop)))
            )
        };
        let messages = release_space(&mut view, &mut tree, &node, &renderer);
        assert_eq!(
            messages.iter().filter(|message| intended(message)).count(),
            1
        );
        let messages = activate(&mut view, &mut tree, &node, &renderer, label);
        assert!(messages.iter().any(intended));
    }
}

#[test]
fn focused_plan_run_ignores_return_and_keeps_explicit_space_activation() {
    let renderer = renderer();
    let mut app = Workbench::new(
        SessionHandle::new(),
        "task-plan-return-tests".into(),
        true,
        None,
    );
    assert!(app.scene_assistant_quick("quick-command-plan", false));
    assert!(app.assistant.quick.ready_to_run());
    let mut view = app.assistant_sheet(Palette::of(false), 900.0);
    let mut tree = Tree::new(view.as_widget());
    let node = view.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, Size::new(720.0, 900.0)),
    );
    apply(
        &mut view,
        &mut tree,
        &node,
        &renderer,
        iced::advanced::widget::operation::focusable::focus::<Message>(Id::new(
            "quick-command-run",
        )),
    );
    assert!(
        key(
            &mut view,
            &mut tree,
            &node,
            &renderer,
            keyboard::key::Named::Enter
        )
        .is_empty()
    );
    let messages = release_space(&mut view, &mut tree, &node, &renderer);
    assert!(matches!(
        &messages[..],
        [Message::Assistant(Input::Quick(quick::Input::Run(_)))]
    ));
}
