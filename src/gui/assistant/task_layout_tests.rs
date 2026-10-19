//! Task modes keep their stop control visible, approvals deliberate and every
//! editor keyboard reachable at all supported scales.
use super::*;
use crate::assistant::run::RunState;
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

pub(super) fn verify_retained_controls(state: &tasks::State, confirming: bool) {
    let mut renderer = renderer();
    let quick = quick::State::default();
    for dark in [false, true] {
        for scale in UiScale::ALL {
            let size = Size::new(480.0, 620.0) / scale.factor();
            let p = Palette::of(dark);
            let mut view =
                if state.mode == tasks::Mode::Plan && !state.active() && state.outcome.is_none() {
                    // Match Workbench::assistant_sheet's default Plan-first route.
                    accessibility::mapped(
                        sheet::content_with_banner(
                            &quick,
                            p,
                            size.height,
                            0,
                            task_view::plan_notice(state, p)
                                .map(|notice| accessibility::mapped(notice, quick::Input::Task)),
                            None,
                        ),
                        |input| Message::Assistant(Input::Quick(input)),
                    )
                } else {
                    view(state, p, size.height)
                };
            let mut tree = Tree::new(view.as_widget());
            let node = view.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, size),
            );
            let semantics = snapshot(&mut view, &mut tree, &node, &renderer);
            for label in [
                "Reveal retained dataset 1",
                "Copy retained dataset 1",
                "Remove retained dataset 1",
            ] {
                assert!(!control(&semantics, label).is_disabled(), "{label}");
            }
            if confirming {
                for label in [
                    "Confirm permanent dataset removal",
                    "Cancel dataset removal",
                ] {
                    assert!(!control(&semantics, label).is_disabled(), "{label}");
                }
            }
            assert_focus_traversal_reveals(&mut view, &mut tree, &node, &renderer);
            let id = if confirming {
                "assistant-retained-confirm"
            } else {
                "assistant-retained-inventory"
            };
            let target = if confirming {
                "assistant-retained-keep".into()
            } else {
                format!("assistant-retained-remove-{}", state.retained[0].key)
            };
            apply(
                &mut view,
                &mut tree,
                &node,
                &renderer,
                iced::advanced::widget::operation::focusable::focus::<Message>(Id::from(target)),
            );
            apply(
                &mut view,
                &mut tree,
                &node,
                &renderer,
                accessibility::focus_reveal_operation(iced::window::Id::unique()),
            );
            let semantics = snapshot(&mut view, &mut tree, &node, &renderer);
            inside(
                &control(
                    &semantics,
                    if confirming {
                        "Cancel dataset removal"
                    } else {
                        "Remove retained dataset 1"
                    },
                ),
                size,
                id,
            );
            artifact(
                &format!("{id}-{}-{:?}", if dark { "dark" } else { "light" }, scale),
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

#[test]
fn plan_first_sheet_displays_retained_operation_errors_as_accessible_alerts() {
    let mut app = Workbench::new(
        crate::session::SessionHandle::new(),
        "retained-error-layout".into(),
        true,
        None,
    );
    app.assistant.tasks.retained_open = true;
    let error = "Workspace identity changed. Removal was refused; refresh retained datasets before reviewing it again.";
    app.assistant.tasks.error = Some(error.into());
    assert_eq!(app.assistant.tasks.mode, tasks::Mode::Plan);
    assert!(!app.assistant.tasks.active());
    assert!(app.assistant.tasks.outcome.is_none());
    let mut renderer = renderer();
    for dark in [false, true] {
        for scale in UiScale::ALL {
            let size = Size::new(480.0, 620.0) / scale.factor();
            // Exercise the Workbench's Plan-first branch, which embeds task
            // recovery in the quick sheet rather than task_view::content.
            let mut view = app.assistant_sheet(Palette::of(dark), size.height);
            let mut tree = Tree::new(view.as_widget());
            let node = view.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, size),
            );
            let semantics = snapshot(&mut view, &mut tree, &node, &renderer);
            let alert = control(&semantics, error);
            assert_eq!(alert.role(), accessibility::Role::Alert);
            assert!(
                semantics
                    .nodes
                    .iter()
                    .all(|(_, node)| node.label() != Some("No retained task datasets.")),
                "an inventory failure cannot be presented as a successful empty inventory"
            );
            assert!(!control(&semantics, "Review retained task datasets").is_disabled());
            assert_focus_traversal_reveals(&mut view, &mut tree, &node, &renderer);
            apply(
                &mut view,
                &mut tree,
                &node,
                &renderer,
                iced::advanced::widget::operation::scrollable::scroll_to::<Message>(
                    Id::new(SHEET_BODY),
                    iced::advanced::widget::operation::scrollable::AbsoluteOffset {
                        x: Some(0.0),
                        y: Some(0.0),
                    },
                ),
            );
            let semantics = snapshot(&mut view, &mut tree, &node, &renderer);
            inside(&control(&semantics, error), size, "retained-plan-error");
            artifact(
                &format!(
                    "assistant-retained-plan-error-{}-{:?}",
                    if dark { "dark" } else { "light" },
                    scale
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

#[test]
fn task_source_profile_editor_keeps_credentials_masked_and_keyboard_reachable() {
    let mut renderer = renderer();
    for dark in [false, true] {
        for scale in UiScale::ALL {
            for physical in [Size::new(480.0, 620.0), Size::new(1100.0, 700.0)] {
                let size = physical / scale.factor();
                let state = task_scenes::fixture(
                    "quick-command-task-data-auth",
                    SessionSnapshot::default(),
                );
                let mut view = view(&state, Palette::of(dark), size.height);
                let mut tree = Tree::new(view.as_widget());
                let node = view.as_widget_mut().layout(
                    &mut tree,
                    &renderer,
                    &layout::Limits::new(Size::ZERO, size),
                );
                let snapshot = snapshot(&mut view, &mut tree, &node, &renderer);
                let password = &snapshot
                    .nodes
                    .iter()
                    .find(|(_, node)| {
                        node.label() == Some("Source password")
                            && node.role() == accessibility::Role::PasswordInput
                    })
                    .unwrap()
                    .1;
                assert_eq!(password.role(), accessibility::Role::PasswordInput);
                assert!(
                    password
                        .value()
                        .is_none_or(|value| !value.contains("fixture-only-password"))
                );
                assert!(!control(&snapshot, "Save protected source profile").is_disabled());
                assert!(control(&snapshot, "Start assistant task").is_disabled());
                assert_focus_traversal_reveals(&mut view, &mut tree, &node, &renderer);
                if scale == UiScale::Standard {
                    apply(
                        &mut view,
                        &mut tree,
                        &node,
                        &renderer,
                        iced::advanced::widget::operation::focusable::focus::<Message>(Id::new(
                            "task-source-profile-save",
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
                            "task-source-profile-password",
                        )),
                    );
                    artifact(
                        &format!(
                            "quick-command-task-data-auth-{}-{}x{}",
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
fn data_and_mixed_task_scenes_keep_intervention_controls_visible_at_every_scale() {
    let mut renderer = renderer();
    for scene in task_scenes::DATA_WORDS {
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
                    for label in [
                        "Close Quick Command",
                        "How the assistant works",
                        "Assistant task provider and model",
                    ] {
                        inside(&control(&snapshot, label), size, scene);
                    }
                    if state.active() {
                        let takeover = if state.cameras.is_empty() {
                            "Take over data capture control"
                        } else {
                            "Take over camera control"
                        };
                        for label in [
                            "Stop assistant task",
                            takeover,
                            if matches!(
                                state.snapshot.state,
                                RunState::Paused | RunState::NeedsUser
                            ) {
                                "Resume assistant task"
                            } else {
                                "Pause assistant task"
                            },
                        ] {
                            let control = control(&snapshot, label);
                            inside(&control, size, scene);
                            assert!(
                                !control.is_disabled()
                                    || label == "Resume assistant task"
                                        && state.data_scope_issue().is_some(),
                                "{scene}: {label}"
                            );
                        }
                    } else if state.outcome.is_some() {
                        for label in ["Export assistant task log", "New assistant task"] {
                            inside(&control(&snapshot, label), size, scene);
                        }
                    } else {
                        inside(&control(&snapshot, "Start assistant task"), size, scene);
                        assert!(
                            snapshot.nodes.iter().any(|(_, node)| node.label()
                                == Some("Assistant task data source endpoint"))
                        );
                        assert!(snapshot.nodes.iter().any(|(_, node)| node.label()
                            == Some(
                                "Allow source values and dataset results to reach the AI provider"
                            )));
                    }
                    if state.approval.is_some() {
                        for label in ["Approve assistant action", "Deny assistant action"] {
                            inside(&control(&snapshot, label), size, scene);
                        }
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
fn data_scope_and_connection_approval_are_keyboard_reachable_in_compact_sheets() {
    let renderer = renderer();
    for scene in [
        "quick-command-task-data-setup",
        "quick-command-task-data-approval",
    ] {
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

#[test]
fn separate_task_dataset_choice_is_explicit_and_keyboard_reachable_in_compact_sheets() {
    let renderer = renderer();
    for dark in [false, true] {
        let mut state =
            task_scenes::fixture("quick-command-task-data-setup", SessionSnapshot::default());
        state.context.as_mut().unwrap().task_data_available = true;
        state.dataset = None;
        state.separate_dataset = true;
        let mut view = view(&state, Palette::of(dark), 620.0);
        let mut tree = Tree::new(view.as_widget());
        let node = view.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(480.0, 620.0)),
        );
        let semantics = snapshot(&mut view, &mut tree, &node, &renderer);
        let choice = control(&semantics, "Separate dataset for this task");
        assert_eq!(choice.toggled(), Some(accesskit::Toggled::True));
        assert!(!choice.is_disabled());
        assert!(semantics.nodes.iter().any(|(_, node)| {
            node.label().is_some_and(|label| {
                label.contains("logical file extent cap")
                    && label.contains("until the dataset is removed")
            })
        }));
        inside(
            &control(&semantics, "Start assistant task"),
            Size::new(480.0, 620.0),
            "separate-dataset-setup",
        );
        assert_focus_traversal_reveals(&mut view, &mut tree, &node, &renderer);
    }
}

#[test]
fn separate_dataset_native_scenes_keep_sensor_scope_and_final_local_receipt() {
    let setup = task_scenes::fixture(
        "quick-command-task-separate-setup",
        SessionSnapshot::default(),
    );
    assert!(setup.separate_dataset && setup.cameras.is_empty() && setup.dataset.is_none());
    assert!(setup.outcome.is_none());
    assert!(setup.context.as_ref().unwrap().snapshot.signals.is_empty());
    let done = task_scenes::fixture(
        "quick-command-task-separate-done",
        SessionSnapshot::default(),
    );
    assert!(done.separate_dataset && done.cameras.is_empty() && done.dataset.is_none());
    let receipt = done.outcome.as_ref().unwrap().task_data.as_ref().unwrap();
    assert_eq!(
        receipt.phase,
        crate::session::task_data::TaskDataPhase::Retained
    );
    assert!(receipt.catalog.closed && !receipt.catalog.quarantined);
    assert_eq!(receipt.catalog.dataset.samples, 400);
    assert_eq!(
        receipt.catalog.extents.limit,
        done.task_dataset_cap().unwrap()
    );
    assert!(receipt.catalog.dataset.path.ends_with("/task.duckdb"));
    assert!(
        done.outcome
            .as_ref()
            .unwrap()
            .summary
            .contains("sensor-only")
    );
}

fn task_dataset_receipt(failed: bool) -> crate::session::task_data::TaskDataStatus {
    use crate::{
        dataset::{
            DatasetStatus,
            task::{ExtentUsage, TaskCatalogIdentity, TaskCatalogStatus},
        },
        session::task_data::{TaskDataPhase, TaskDataStatus},
    };
    let identity = TaskCatalogIdentity {
        dataset_id: "a".repeat(16),
        slot_id: "b".repeat(32),
    };
    TaskDataStatus {
        id: identity.slot_id.clone(),
        phase: if failed {
            TaskDataPhase::RetainedError
        } else {
            TaskDataPhase::Retained
        },
        catalog: TaskCatalogStatus {
            identity: identity.clone(),
            dataset: DatasetStatus {
                path: "/captures/task-0123456789abcdef0123456789abcdef/task.duckdb".into(),
                dataset_id: identity.dataset_id,
                slot_id: Some(identity.slot_id),
                samples: 398,
                signals: 2,
                events: 4,
                ..Default::default()
            },
            extents: ExtentUsage {
                limit: 8 * 1024 * 1024,
                used: 3 * 1024 * 1024,
                ..Default::default()
            },
            quarantined: failed,
            closed: !failed,
        },
        sink: Default::default(),
        captures: Default::default(),
        error: failed
            .then(|| "Task dataset checkpoint failed; files and reservation retained.".into()),
    }
}

#[test]
fn retained_task_dataset_path_counters_and_failure_receipt_wrap_in_compact_layouts() {
    let mut renderer = renderer();
    for (failed, dropped) in [(false, 0), (true, 0), (false, 2)] {
        for dark in [false, true] {
            for scale in UiScale::ALL {
                let size = Size::new(480.0, 620.0) / scale.factor();
                let mut state = task_scenes::fixture(
                    "quick-command-task-data-done",
                    SessionSnapshot::default(),
                );
                state.separate_dataset = true;
                let outcome = state.outcome.as_mut().unwrap();
                outcome.signal_reports.clear();
                outcome.task_data = Some(task_dataset_receipt(failed));
                outcome.task_data.as_mut().unwrap().sink.dropped_total = dropped;
                let mut view = accessibility::mapped(
                    task_view::data_progress(&state, Palette::of(dark)),
                    |input| Message::Assistant(Input::Task(input)),
                );
                let mut tree = Tree::new(view.as_widget());
                let narrow = Size::new(240.0, size.height);
                let node = view.as_widget_mut().layout(
                    &mut tree,
                    &renderer,
                    &layout::Limits::new(Size::ZERO, narrow),
                );
                let semantics = snapshot(&mut view, &mut tree, &node, &renderer);
                for label in [
                    "Task dataset",
                    "/captures/task-0123456789abcdef0123456789abcdef/task.duckdb",
                    "398 stored samples · 2 signals · 4 events",
                    if failed || dropped > 0 {
                        "Dataset persistence or shutdown needs review"
                    } else {
                        "Dataset closed and retained"
                    },
                ] {
                    inside(&control(&semantics, label), narrow, "retained-task-dataset");
                }
                if failed {
                    inside(
                        &control(
                            &semantics,
                            "Task dataset checkpoint failed; files and reservation retained.",
                        ),
                        narrow,
                        "retained-task-dataset-error",
                    );
                }
                if dropped > 0 {
                    inside(
                        &control(&semantics, "Task writer reports 2 dropped samples."),
                        narrow,
                        "retained-task-dataset-drops",
                    );
                }
                artifact(
                    &format!(
                        "task-dataset-receipt-{}-{dropped}drops-{}-{:?}",
                        if failed { "failed" } else { "closed" },
                        if dark { "dark" } else { "light" },
                        scale
                    ),
                    &mut view,
                    &mut tree,
                    &node,
                    &mut renderer,
                    narrow,
                    (dark, scale.factor()),
                );
            }
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

#[cfg(feature = "s3")]
#[test]
fn human_only_destination_approval_is_secure_and_never_offers_task_scope() {
    let renderer = renderer();
    let mut state = task_scenes::fixture("quick-command-task-approval", SessionSnapshot::default());
    state.approval = Some(task_scenes::secret_approval(
        state.context.as_ref().unwrap(),
    ));
    state.approval_secret = settings::SecretDraft::from("never-expose-this-key".to_owned());
    let mut view = view(&state, Palette::of(false), 900.0);
    let mut tree = Tree::new(view.as_widget());
    let node = view.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, Size::new(720.0, 900.0)),
    );
    let snapshot = snapshot(&mut view, &mut tree, &node, &renderer);
    let secret = control(&snapshot, "Assistant approval secret key");
    assert_eq!(secret.role(), accessibility::Role::PasswordInput);
    assert!(!format!("{snapshot:?}").contains("never-expose-this-key"));
    assert!(
        !format!(
            "{:?}",
            tasks::Input::ApprovalSecret(state.approval_secret.clone())
        )
        .contains("never-expose-this-key")
    );
    assert!(
        !snapshot
            .nodes
            .iter()
            .any(|(_, node)| node.label() == Some("Approve this action for this task"))
    );
    assert!(!control(&snapshot, "Approve assistant action").is_disabled());
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
