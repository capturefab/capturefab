use super::*;
use iced::advanced::{Layout, layout, widget::Tree};
use iced::futures::StreamExt;

fn app() -> Workbench {
    let mut app = Workbench::new(SessionHandle::new(), "review-semantics".into(), true, None);
    app.width = 900.0;
    app.height = 620.0;
    app.snapshot.capture_session = Some(crate::autocapture::SessionStatus {
        path: "/isolated/review.duckdb".into(),
        camera: "sim:0".into(),
        enabled: true,
        sensitivity: 0.5,
        cooldown_ms: 2000,
        manual_count: 1,
        kept_count: 0,
        discarded_count: 0,
        pending_count: 3,
        blocked_storage: false,
        state: "watching".into(),
        model: "Session exemplar kNN + background novelty".into(),
        encoder: "native".into(),
        accelerator: None,
        samples_per_second: None,
        note: None,
    });
    app.sync_capture_session();
    app.auto_capture.open = true;
    app.auto_capture.review_context = app.capture_session_context();
    app.sheets.shown.set(1.0);
    app
}

fn card(id: u64) -> Card {
    Card {
        id,
        score: 0.87,
        reason: "Similar to your kept examples; unusual against the recent background.".into(),
        image: image::Handle::from_rgba(480, 360, vec![127; 480 * 360 * 4]),
    }
}

fn collect(
    app: &Workbench,
    tree: &mut Tree,
    renderer: &iced::Renderer,
    focus: Option<&'static str>,
) -> (accesskit::TreeUpdate, accessibility::Bridge, window::Id) {
    // Exercise the actual shared SheetFrame and the modal's background guard.
    let base = accessibility::label(
        text("Underlying toolbar failure").into(),
        "Underlying toolbar failure".into(),
        accessibility::Role::Alert,
    );
    let mut view = app.sheets(base, Palette::of(false));
    tree.diff(view.as_widget());
    let node = view.as_widget_mut().layout(
        tree,
        renderer,
        &layout::Limits::new(Size::ZERO, Size::new(app.width, app.height)),
    );
    if let Some(focus) = focus {
        crate::gui::destinations::layout_tests::apply(
            &mut view,
            tree,
            &node,
            renderer,
            iced::advanced::widget::operation::focusable::focus(focus.into()),
        );
    }
    accessibility::snapshot_widget(view.as_widget_mut(), tree, Layout::new(&node), renderer)
}

fn named<'a>(
    snapshot: &'a accesskit::TreeUpdate,
    name: &str,
) -> (accessibility::NodeId, &'a accessibility::Node) {
    snapshot
        .nodes
        .iter()
        .find(|(_, node)| node.label() == Some(name))
        .map(|(id, node)| (*id, node))
        .unwrap_or_else(|| panic!("missing native node {name}"))
}

fn settings_snapshot(
    app: &Workbench,
    renderer: &iced::Renderer,
) -> (accesskit::TreeUpdate, accessibility::Bridge, window::Id) {
    let mut view = app.auto_capture_settings(Palette::of(false));
    let mut tree = Tree::new(view.as_widget());
    let node = view.as_widget_mut().layout(
        &mut tree,
        renderer,
        &layout::Limits::new(Size::ZERO, Size::new(INSPECTOR - 38.0, 1600.0)),
    );
    accessibility::snapshot_widget(
        view.as_widget_mut(),
        &mut tree,
        Layout::new(&node),
        renderer,
    )
}

#[test]
fn inactive_automatic_capture_reveals_setup_through_a_native_disclosure() {
    let renderer = crate::gui::destinations::layout_tests::renderer();
    let mut app = Workbench::new(SessionHandle::new(), "capture-setup".into(), true, None);
    let (snapshot, bridge, window) = settings_snapshot(&app, &renderer);
    let (id, disclosure) = named(&snapshot, "Automatic capture");
    assert_eq!(disclosure.role(), accessibility::Role::DisclosureTriangle);
    assert_eq!(disclosure.is_expanded(), Some(false));
    assert_eq!(disclosure.description(), Some("Teach & review"));
    assert!(
        !snapshot
            .nodes
            .iter()
            .any(|(_, node)| node.label() == Some("New session"))
    );
    let bounds = disclosure.bounds().unwrap();
    assert!(bounds.x0 >= 0.0 && bounds.x1 <= f64::from(INSPECTOR - 38.0));

    let mut actions = iced_runtime::task::into_stream(bridge.dispatch(accessibility::Request {
        window,
        replay: None,
        action: accesskit::ActionRequest {
            action: accesskit::Action::Click,
            target: id,
            data: None,
        },
    }))
    .unwrap();
    let event = match iced::futures::executor::block_on(actions.next()) {
        Some(iced_runtime::Action::Output(Message::AutoCapture(event @ Input::Setup(true)))) => {
            event
        }
        _ => panic!("disclosure opens automatic capture setup"),
    };
    drop(app.update_autocapture(event));
    let (snapshot, _, _) = settings_snapshot(&app, &renderer);
    assert_eq!(
        named(&snapshot, "Automatic capture").1.is_expanded(),
        Some(true)
    );
    for label in ["New session", "Resume…"] {
        assert_eq!(
            named(&snapshot, label).1.role(),
            accessibility::Role::Button
        );
    }
    drop(app.update_autocapture(Input::Setup(false)));
    let (snapshot, _, _) = settings_snapshot(&app, &renderer);
    assert!(
        !snapshot
            .nodes
            .iter()
            .any(|(_, node)| node.label() == Some("New session"))
    );
    assert!(
        app.pending.is_empty(),
        "disclosure has no camera side effects"
    );
}

#[test]
fn a_session_note_says_why_a_suggestion_was_skipped() {
    let renderer = crate::gui::destinations::layout_tests::renderer();
    let mut app = app();
    app.auto_capture.setup_open = false;
    let note = "Skipped a suggestion while the previous capture was still being saved";
    app.snapshot.capture_session.as_mut().unwrap().note = Some(note.into());
    let (snapshot, _, _) = settings_snapshot(&app, &renderer);
    named(&snapshot, note);
    named(&snapshot, state_guidance("watching"));
}

#[test]
fn active_capture_sessions_keep_reviews_counts_and_capacity_guidance_visible() {
    let renderer = crate::gui::destinations::layout_tests::renderer();
    let mut app = app();
    app.auto_capture.setup_open = false;
    app.snapshot.capture_session.as_mut().unwrap().state = "review_queue_full".into();
    let (snapshot, bridge, window) = settings_snapshot(&app, &renderer);
    for label in [
        "Capture example",
        "Review suggestions (3)",
        "1 example · 0 kept · 0 discarded",
        state_guidance("review_queue_full"),
        "Save & close session",
    ] {
        named(&snapshot, label);
    }
    assert!(!snapshot.nodes.iter().any(|(_, node)| {
        node.role() == accessibility::Role::DisclosureTriangle
            && node.label() == Some("Automatic capture")
    }));
    let (_, review) = named(&snapshot, "Review suggestions (3)");
    let (_, teach) = named(&snapshot, "Capture example");
    let (options_id, options) = named(&snapshot, "Suggestion settings");
    assert_eq!(options.is_expanded(), Some(false));
    assert_eq!(options.description(), Some("Balanced · 2 s"));
    assert!(review.bounds().unwrap().y1 <= teach.bounds().unwrap().y0);
    assert!(teach.bounds().unwrap().y1 <= options.bounds().unwrap().y0);
    assert!(
        !snapshot
            .nodes
            .iter()
            .any(|(_, node)| node.label() == Some("Auto capture sensitivity"))
    );
    let mut actions = iced_runtime::task::into_stream(bridge.dispatch(accessibility::Request {
        window,
        replay: None,
        action: accesskit::ActionRequest {
            action: accesskit::Action::Click,
            target: options_id,
            data: None,
        },
    }))
    .unwrap();
    match iced::futures::executor::block_on(actions.next()) {
        Some(iced_runtime::Action::Output(Message::AutoCapture(event @ Input::Options(true)))) => {
            drop(app.update_autocapture(event));
        }
        _ => panic!("suggestion settings opens through the native disclosure"),
    }
    let (snapshot, _, _) = settings_snapshot(&app, &renderer);
    for label in [
        "Auto capture sensitivity",
        "Minimum gap between auto captures",
        state_guidance("review_queue_full"),
    ] {
        named(&snapshot, label);
    }
    assert!(
        !snapshot
            .nodes
            .iter()
            .any(|(_, node)| node.label() == Some("New session"))
    );
}

#[test]
fn review_image_and_live_outcomes_belong_to_the_dialog_and_preserve_controls() {
    let renderer = crate::gui::destinations::layout_tests::renderer();
    let mut app = app();
    let mut tree = Tree::empty();
    let error = "Could not save the decision: session storage is unavailable";
    let mut control_ids = None;
    let mut outcome_id = None;
    let mut image_id = None;
    let names = [
        "Close Review auto captures",
        "Discard capture, left arrow",
        "Keep capture, right arrow",
    ];
    for phase in 0..7 {
        app.pending.clear();
        app.auto_capture.error = None;
        match phase {
            0 => app.pending.push(Pending::unanswered(Job::CaptureReview)),
            1 => app.auto_capture.card = Some(card(24)),
            2 => app.pending.push(Pending::unanswered(Job::ReviewCapture)),
            3 => app.auto_capture.error = Some(error.into()),
            4 => app.pending.push(Pending::unanswered(Job::CaptureReview)),
            5 => {
                app.auto_capture.card = Some(card(25));
                app.auto_capture.reviewed = 1;
            }
            _ => app.auto_capture.card = None,
        }
        let (snapshot, bridge, window) = collect(
            &app,
            &mut tree,
            &renderer,
            (phase == 0).then_some("sheet-close"),
        );
        // The Dialog and its visible Heading intentionally share their name.
        let (dialog_id, dialog) = snapshot
            .nodes
            .iter()
            .find(|(_, node)| {
                node.role() == accessibility::Role::Dialog
                    && node.label() == Some("Review auto captures")
            })
            .map(|(id, node)| (*id, node))
            .expect("review owns a named modal Dialog");
        assert!(dialog.is_modal());
        let ids = names.map(|name| named(&snapshot, name).0);
        if let Some(previous) = control_ids {
            assert_eq!(
                ids, previous,
                "async outcomes must not replace action state"
            );
        }
        control_ids = Some(ids);
        assert_eq!(snapshot.focus, ids[0]);
        assert!(
            !snapshot
                .nodes
                .iter()
                .any(|(_, node)| { node.label() == Some("Underlying toolbar failure") })
        );

        let live: Vec<_> = snapshot
            .nodes
            .iter()
            .filter(|(_, node)| node.live().is_some())
            .collect();
        assert_eq!(
            live.len(),
            1,
            "one active review outcome owns announcements"
        );
        let (id, outcome) = live[0];
        if let Some(previous) = outcome_id {
            assert_eq!(*id, previous);
        }
        outcome_id = Some(*id);
        assert_eq!(outcome.value(), outcome.label());
        assert!(!outcome.supports_action(accesskit::Action::Focus));
        assert!(!outcome.supports_action(accesskit::Action::Click));
        assert!(dialog.children().contains(id));
        let expected = match phase {
            0 | 4 => "Loading next capture…",
            1 => "Capture 24 ready. Swipe or use ← / →.",
            2 => "Saving your decision…",
            3 => {
                "Capture review failed: Could not save the decision: session storage is unavailable"
            }
            5 => "Decision saved. Capture 25 ready. Swipe or use ← / →.",
            _ => "Decision saved. You're caught up.",
        };
        assert_eq!(outcome.value(), Some(expected));
        assert_eq!(
            outcome.role(),
            if phase == 3 {
                accessibility::Role::Alert
            } else {
                accessibility::Role::Status
            }
        );
        assert_eq!(
            outcome.live(),
            Some(if phase == 3 {
                accesskit::Live::Assertive
            } else {
                accesskit::Live::Polite
            })
        );
        if phase == 3 {
            assert_eq!(
                snapshot
                    .nodes
                    .iter()
                    .filter(|(_, node)| node.label().is_some_and(|text| text.contains(error)))
                    .count(),
                1,
                "the complete error belongs to its active Alert once"
            );
            let retry = named(&snapshot, "Try again").0;
            let mut actions =
                iced_runtime::task::into_stream(bridge.dispatch(accessibility::Request {
                    window,
                    replay: None,
                    action: accesskit::ActionRequest {
                        action: accesskit::Action::Click,
                        target: retry,
                        data: None,
                    },
                }))
                .unwrap();
            assert!(matches!(
                iced::futures::executor::block_on(actions.next()),
                Some(iced_runtime::Action::Output(Message::AutoCapture(
                    Input::Retry
                )))
            ));
        }
        let images: Vec<_> = snapshot
            .nodes
            .iter()
            .filter(|(_, node)| node.role() == accessibility::Role::Image)
            .collect();
        if let Some(card) = &app.auto_capture.card {
            assert_eq!(images.len(), 1);
            let (id, image) = images[0];
            assert_eq!(
                image.label(),
                Some(format!("Suggested capture {} preview", card.id).as_str())
            );
            assert!(image.description().unwrap().contains("Use Keep or Discard"));
            assert!(dialog.children().contains(id));
            assert!(!image.supports_action(accesskit::Action::Focus));
            assert!(!image.supports_action(accesskit::Action::Click));
            if let Some(previous) = image_id {
                assert_eq!(*id, previous);
            }
            image_id = Some(*id);
        } else {
            assert!(images.is_empty());
        }
        assert_eq!(
            snapshot
                .nodes
                .iter()
                .filter(|(_, node)| node.children().contains(&dialog_id))
                .count(),
            1
        );
    }

    // Pointer movement and newly arriving suggestions do not announce a
    // different outcome while the same card remains under review.
    app.auto_capture.card = Some(card(26));
    let (before, _, _) = collect(&app, &mut tree, &renderer, None);
    app.auto_capture.pointer = Some(Point::new(150.0, 40.0));
    app.auto_capture.drag_start = Some(Point::new(10.0, 40.0));
    app.snapshot.capture_session.as_mut().unwrap().pending_count += 1;
    let (after, _, _) = collect(&app, &mut tree, &renderer, None);
    let outcome = |snapshot: &accesskit::TreeUpdate| {
        snapshot
            .nodes
            .iter()
            .find(|(_, node)| node.live().is_some())
            .map(|(id, node)| (*id, node.value().unwrap().to_owned()))
            .unwrap()
    };
    assert_eq!(outcome(&before), outcome(&after));
}

#[test]
fn long_review_errors_scroll_without_displacing_compact_header_or_decisions() {
    use iced::Rectangle;
    use iced::advanced::widget::{Id, Operation};
    #[derive(Default)]
    struct Controls(Vec<(String, Rectangle)>);
    impl Operation for Controls {
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
            operate(self);
        }
        fn custom(&mut self, _: Option<&Id>, bounds: Rectangle, state: &mut dyn std::any::Any) {
            if let Some(control) = state.downcast_ref::<accessibility::Control<Message>>()
                && let Some(name) = control.node.label()
                && matches!(
                    name,
                    "Close Review auto captures"
                        | "Discard capture, left arrow"
                        | "Keep capture, right arrow"
                )
            {
                self.0.push((name.into(), bounds));
            }
        }
    }
    let renderer = crate::gui::destinations::layout_tests::renderer();
    for (native, scale) in [
        (Size::new(900.0, 620.0), prefs::UiScale::Larger),
        (Size::new(640.0, 360.0), prefs::UiScale::Standard),
        (Size::new(640.0, 360.0), prefs::UiScale::Large),
        (Size::new(640.0, 360.0), prefs::UiScale::Larger),
    ] {
        let size = Size::new(
            native.width / scale.factor(),
            native.height / scale.factor(),
        );
        let mut app = app();
        app.width = size.width;
        app.height = size.height;
        app.auto_capture.card = Some(card(24));
        let error = format!(
            "Could not write /{}.duckdb: storage unavailable",
            "long-unbroken-path".repeat(160)
        );
        app.auto_capture.error = Some(error.clone());
        let mut view = app.auto_capture_review(Palette::of(false), (size.height - 80.0).max(320.0));
        let mut tree = Tree::new(view.as_widget());
        let node = view.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, size),
        );
        let mut controls = Controls::default();
        view.as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut controls);
        assert_eq!(controls.0.len(), 3);
        for (name, bounds) in controls.0 {
            assert!(
                bounds.width > 0.0
                    && bounds.height > 0.0
                    && bounds.x >= -0.5
                    && bounds.y >= -0.5
                    && bounds.x + bounds.width <= size.width + 0.5
                    && bounds.y + bounds.height <= size.height + 0.5,
                "{name} remains inside {native:?} at {scale:?}: {bounds:?}"
            );
        }
        let (snapshot, _, _) = accessibility::snapshot_widget(
            view.as_widget_mut(),
            &mut tree,
            Layout::new(&node),
            &renderer,
        );
        let full = format!("Capture review failed: {error}");
        let (_, alert) = named(&snapshot, &full);
        assert_eq!(alert.value(), Some(full.as_str()));
        assert_eq!(alert.live(), Some(accesskit::Live::Assertive));
    }
}

#[test]
fn review_keys_and_queued_decisions_cannot_act_through_a_failed_close_alert() {
    let renderer = crate::gui::destinations::layout_tests::renderer();
    let mut app = app();
    app.auto_capture.card = Some(card(24));
    app.auto_capture.reviewed = 3;
    drop(app.show_close_error(CloseTarget::Quit, "isolated settings write failure".into()));
    for key in [keys::Key::Left, keys::Key::Right] {
        drop(app.update(Message::Key {
            chord: Chord {
                key,
                mods: keys::Mods::NONE,
            },
            captured: false,
        }));
    }
    drop(app.update(Message::AutoCapture(Input::Decide(false))));
    app.auto_capture.drag_start = Some(Point::ORIGIN);
    app.auto_capture.pointer = Some(Point::new(150.0, 0.0));
    drop(app.update(Message::AutoCapture(Input::DragEnd)));
    assert!(app.pending.is_empty());
    assert_eq!(app.auto_capture.card.as_ref().unwrap().id, 24);
    assert_eq!(app.auto_capture.reviewed, 3);
    assert!(app.handle.activity().activity.is_empty());
    let mut view = app.close_error_view(
        app.auto_capture_review(Palette::of(false), 540.0),
        app.main_window,
    );
    let mut tree = Tree::new(view.as_widget());
    let node = view.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, Size::new(900.0, 620.0)),
    );
    let (snapshot, _, _) = accessibility::snapshot_widget(
        view.as_widget_mut(),
        &mut tree,
        Layout::new(&node),
        &renderer,
    );
    let alert = snapshot
        .nodes
        .iter()
        .find(|(_, node)| node.role() == accessibility::Role::AlertDialog)
        .unwrap()
        .1
        .clone();
    for name in ["Cancel", "Retry"] {
        let (id, button) = named(&snapshot, name);
        assert!(button.supports_action(accesskit::Action::Click));
        assert!(alert.children().contains(&id));
    }
    assert!(
        !snapshot
            .nodes
            .iter()
            .any(|(_, node)| node.label() == Some("Keep capture, right arrow"))
    );
    drop(view);
    app.dismiss_close_error();
    app.closing = Some(CloseTarget::Quit);
    app.decide_capture(true);
    assert!(app.pending.is_empty());
    assert!(app.handle.activity().activity.is_empty());

    for (key, verb) in [(keys::Key::Left, "Discard"), (keys::Key::Right, "Keep")] {
        let mut app = self::app();
        app.auto_capture.card = Some(card(24));
        drop(app.update(Message::Key {
            chord: Chord {
                key,
                mods: keys::Mods::NONE,
            },
            captured: false,
        }));
        assert_eq!(app.pending.len(), 1);
        assert_eq!(app.pending[0].job, Job::ReviewCapture);
        let activity = app.handle.activity().activity;
        assert_eq!(activity.len(), 1);
        assert_eq!(activity[0].label, format!("{verb} auto capture 24"));
    }
}
