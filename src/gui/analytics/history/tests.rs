use super::*;
use iced::advanced::{Layout, layout, widget::Tree};
use iced::futures::StreamExt;

fn report(count: usize) -> Arc<AnalyticsSnapshot> {
    let first = 1_700_000_060_000;
    let points: Vec<_> = (0..count)
        .map(|index| AnalyticsPoint {
            timestamp_ms: first + index as u64 * 2500,
            frames: (index + 1) as u64 * 10,
            dropped: index as u64 % 3,
            bytes: (index + 1) as u64 * 1024,
            fps: (index + 1) as f64 * 2.0,
            bytes_per_second: (index + 1) as f64 * 1_048_576.0,
            drop_percent: (index % 3) as f64 * 5.0,
            lost: index as u64 % 3,
            lost_percent: (index % 3) as f64 * 5.0,
        })
        .collect();
    Arc::new(AnalyticsSnapshot {
        retention_seconds: 86_400,
        sample_interval_seconds: 1,
        started_at_ms: first - 60_000,
        collected_at_ms: first + count as u64 * 2500,
        samples: count,
        truncated: false,
        totals: crate::analytics::AnalyticsTotals::default(),
        cameras: vec![crate::analytics::CameraAnalytics {
            id: "sim:0".into(),
            name: "Workshop camera".into(),
            frames: 100,
            dropped: 2,
            bytes: 2_097_152,
            fps: 10.0,
            bytes_per_second: 1_048_576.0,
            drop_percent: 2.0,
            streaming: true,
            stale: false,
            lost: 2,
            preview_skipped: 0,
            lost_percent: 2.0,
        }],
        points,
    })
}

fn bench(report: Arc<AnalyticsSnapshot>) -> Workbench {
    let mut bench = Workbench::new(SessionHandle::new(), "history-tree".into(), true, None);
    bench.width = 900.0;
    bench.analytics.open = true;
    bench.analytics.report = Some(report);
    bench.analytics.chart = Some(super::super::chart_fixture());
    bench
}

fn collect(
    bench: &Workbench,
    tree: &mut Tree,
    renderer: &iced::Renderer,
) -> (accesskit::TreeUpdate, accessibility::Bridge, window::Id) {
    let mut view = bench.analytics_sheet(Palette::of(false), 700.0);
    tree.diff(view.as_widget());
    let node = view.as_widget_mut().layout(
        tree,
        renderer,
        &layout::Limits::new(Size::ZERO, Size::new(900.0, 700.0)),
    );
    accessibility::snapshot_widget(view.as_widget_mut(), tree, Layout::new(&node), renderer)
}

fn named<'a>(
    tree: &'a accesskit::TreeUpdate,
    name: &str,
) -> (accessibility::NodeId, &'a accessibility::Node) {
    tree.nodes
        .iter()
        .find(|(_, node)| node.label() == Some(name))
        .map(|(id, node)| (*id, node))
        .unwrap_or_else(|| panic!("missing native node {name}"))
}

fn rows(tree: &accesskit::TreeUpdate) -> Vec<(accessibility::NodeId, &accessibility::Node)> {
    tree.nodes
        .iter()
        .filter(|(_, node)| {
            node.role() == accessibility::Role::Group
                && node
                    .label()
                    .is_some_and(|name| name.starts_with("History point "))
        })
        .map(|(id, node)| (*id, node))
        .collect()
}

fn plot_ids(tree: &accesskit::TreeUpdate) -> Vec<accessibility::NodeId> {
    let plots: Vec<_> = tree
        .nodes
        .iter()
        .filter(|(_, node)| node.role() == accessibility::Role::Image)
        .map(|(id, _)| *id)
        .collect();
    assert_eq!(plots.len(), 3);
    plots
}

fn native_click(
    bridge: &accessibility::Bridge,
    window: window::Id,
    id: accessibility::NodeId,
) -> Message {
    let task = bridge.dispatch(accessibility::Request {
        window,
        replay: None,
        action: accesskit::ActionRequest {
            action: accesskit::Action::Click,
            target: id,
            data: None,
        },
    });
    let mut actions = iced_runtime::task::into_stream(task).expect("native action is available");
    match iced::futures::executor::block_on(actions.next()) {
        Some(iced_runtime::Action::Output(message)) => message,
        _ => panic!("the native action must dispatch the normal GUI message"),
    }
}

#[test]
fn measured_overview_and_axes_match_the_real_retained_origin_and_plot_domains() {
    let report = report(17);
    let lines = overview(&report);
    assert_eq!(
        lines[1],
        "Frame rate: first 2.00 → last 34.00 frames per second; minimum 2.00; peak 34.00 at +40.000 seconds."
    );
    assert_eq!(
        lines[2],
        "Throughput: first 1.00 → last 17.00 MiB per second; minimum 1.00; peak 17.00 at +40.000 seconds."
    );
    assert_eq!(
        lines[3],
        "Missed frames: first 0.00 → last 5.00 percent; minimum 0.00; peak 10.00 at +35.000 seconds."
    );
    assert!((axis_max(&report, |p| p.fps) - 40.12).abs() < 0.0001);
    let renderer = super::super::super::destinations::layout_tests::renderer();
    let mut bench = bench(report);
    drop(bench.update(Message::AnalyticsHistory(Input::Toggle)));
    let (tree, _, _) = collect(&bench, &mut Tree::empty(), &renderer);
    assert!(
        tree.nodes.iter().any(|(_, node)| node
            .label()
            .is_some_and(|name| name.contains("zero to 40.000 seconds")
                && name.contains("2023-11-14 22:14:20.000 UTC")
                && name.contains("not session start (2023-11-14 22:13:20.000 UTC)")))
    );
    assert!(
        tree.nodes.iter().any(|(_, node)| node
            .label()
            .is_some_and(|name| name.contains("zero to 40.12 frames per second")
                && name.contains("zero to 20.06 MiB per second")
                && name.contains("zero to 11.80 percent")))
    );
    let (_, camera) = named(&tree, "Camera data volume sim:0");
    assert_eq!(camera.children().len(), 1);
    assert!(
        tree.nodes
            .iter()
            .any(|(id, node)| camera.children().contains(id)
                && node.label()
                    == Some("Workshop camera (sim:0) · 2,097,152 session payload bytes"))
    );
    assert!(
        !tree.nodes.iter().any(|(_, node)| node.live().is_some()),
        "chart measurements and history never create periodic live announcements"
    );
}

#[test]
fn actual_native_history_is_bounded_chronological_frozen_and_explicitly_updated() {
    let renderer = super::super::super::destinations::layout_tests::renderer();
    let original = report(17);
    let mut bench = bench(original.clone());
    let mut tree = Tree::empty();
    let (closed, bridge, window) = collect(&bench, &mut tree, &renderer);
    assert!(rows(&closed).is_empty());
    let (toggle, disclosure) = named(&closed, "Show history data");
    assert_eq!(
        closed
            .nodes
            .iter()
            .filter(|(_, node)| node.label() == Some("Show history data"))
            .count(),
        1,
        "the disclosure caption belongs to its native control once"
    );
    assert_eq!(disclosure.role(), accessibility::Role::DisclosureTriangle);
    assert_eq!(disclosure.is_expanded(), Some(false));
    assert!(disclosure.supports_action(accesskit::Action::Click));
    drop(bench.update(native_click(&bridge, window, toggle)));
    let (opened, _, _) = collect(&bench, &mut tree, &renderer);
    let first_rows = rows(&opened);
    assert_eq!(
        opened
            .nodes
            .iter()
            .filter(|(_, node)| node.label() == Some("Hide history data"))
            .count(),
        1
    );
    assert_eq!(first_rows.len(), PAGE_SIZE);
    let original_ids: Vec<_> = first_rows.iter().map(|(id, _)| *id).collect();
    for (index, (id, group)) in first_rows.iter().enumerate() {
        assert_eq!(
            group.label(),
            Some(
                format!(
                    "History point {}",
                    timestamp(original.points[index].timestamp_ms)
                )
                .as_str()
            )
        );
        assert_eq!(group.children().len(), 3);
        assert_eq!(
            opened
                .nodes
                .iter()
                .filter(|(_, node)| node.children().contains(id))
                .count(),
            1
        );
        for child in group.children() {
            assert_eq!(
                opened
                    .nodes
                    .iter()
                    .filter(|(_, node)| node.children().contains(child))
                    .count(),
                1
            );
        }
        assert!(!group.supports_action(accesskit::Action::Focus));
        assert!(!group.supports_action(accesskit::Action::Click));
    }
    let close = named(&opened, "Close Session analytics").0;
    let images = plot_ids(&opened);
    let mut latest = (*original).clone();
    latest.collected_at_ms += 5_000;
    latest.points.remove(0);
    latest.points[0].fps = 99.0;
    bench.analytics.report = Some(Arc::new(latest));
    let (refreshed, bridge, window) = collect(&bench, &mut tree, &renderer);
    assert_eq!(
        rows(&refreshed)
            .iter()
            .map(|(id, _)| *id)
            .collect::<Vec<_>>(),
        original_ids
    );
    assert!(Arc::ptr_eq(
        bench.analytics.history.frozen.as_ref().unwrap(),
        &original
    ));
    assert_eq!(named(&refreshed, "Close Session analytics").0, close);
    assert_eq!(plot_ids(&refreshed), images);
    assert!(refreshed.nodes.iter().any(|(_, node)| node.label()
        == Some("A newer report is available. The displayed snapshot has not changed.")));
    let update = named(&refreshed, "Update history").0;
    drop(bench.update(native_click(&bridge, window, update)));
    let (updated, bridge, window) = collect(&bench, &mut tree, &renderer);
    assert_eq!(
        rows(&updated)[0].0,
        original_ids[1],
        "retained timestamps keep their native IDs on explicit update"
    );
    assert!(!updated.nodes.iter().any(|(id, _)| *id == original_ids[0]));
    assert!(named(&updated, "Update history").1.is_disabled());
    let next = named(&updated, "Next history page").0;
    drop(bench.update(native_click(&bridge, window, next)));
    let (last, bridge, window) = collect(&bench, &mut tree, &renderer);
    assert_eq!(rows(&last).len(), 8);
    assert!(named(&last, "Next history page").1.is_disabled());
    let previous = named(&last, "Previous history page").0;
    drop(bench.update(native_click(&bridge, window, previous)));
    let (returned, bridge, window) = collect(&bench, &mut tree, &renderer);
    assert_eq!(rows(&returned)[0].0, original_ids[1]);
    let hide = named(&returned, "Hide history data").0;
    let task = bench.update(native_click(&bridge, window, hide));
    let mut view = bench.analytics_sheet(Palette::of(false), 700.0);
    tree.diff(view.as_widget());
    let node = view.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, Size::new(900.0, 700.0)),
    );
    let mut actions = iced_runtime::task::into_stream(task).unwrap();
    while let Some(action) = iced::futures::executor::block_on(actions.next()) {
        if let iced_runtime::Action::Widget(mut operation) = action {
            loop {
                view.as_widget_mut().operate(
                    &mut tree,
                    Layout::new(&node),
                    &renderer,
                    operation.as_mut(),
                );
                match operation.finish() {
                    iced::advanced::widget::operation::Outcome::Chain(next) => operation = next,
                    _ => break,
                }
            }
        }
    }
    let (hidden, _, _) = accessibility::snapshot_widget(
        view.as_widget_mut(),
        &mut tree,
        Layout::new(&node),
        &renderer,
    );
    assert!(rows(&hidden).is_empty());
    assert_eq!(
        hidden.focus,
        named(&hidden, "Show history data").0,
        "hiding inspected data returns actual focus to its disclosure"
    );
    assert_eq!(named(&hidden, "Show history data").0, toggle);
}

#[test]
fn refreshing_snapshot_notices_retains_native_pager_ids_and_actual_focus() {
    fn after_task(
        bench: &Workbench,
        tree: &mut Tree,
        renderer: &iced::Renderer,
        task: Task<Message>,
    ) -> (accesskit::TreeUpdate, accessibility::Bridge, window::Id) {
        let mut view = bench.analytics_sheet(Palette::of(false), 700.0);
        tree.diff(view.as_widget());
        let node = view.as_widget_mut().layout(
            tree,
            renderer,
            &layout::Limits::new(Size::ZERO, Size::new(900.0, 700.0)),
        );
        if let Some(mut actions) = iced_runtime::task::into_stream(task) {
            while let Some(action) = iced::futures::executor::block_on(actions.next()) {
                if let iced_runtime::Action::Widget(mut operation) = action {
                    loop {
                        view.as_widget_mut().operate(
                            tree,
                            Layout::new(&node),
                            renderer,
                            operation.as_mut(),
                        );
                        match operation.finish() {
                            iced::advanced::widget::operation::Outcome::Chain(next) => {
                                operation = next
                            }
                            _ => break,
                        }
                    }
                }
            }
        }
        accessibility::snapshot_widget(view.as_widget_mut(), tree, Layout::new(&node), renderer)
    }

    let renderer = super::super::super::destinations::layout_tests::renderer();
    let pager_names = ["History page", "Previous history page", "Next history page"];
    for (focus, focus_name) in [PAGE, PREVIOUS, NEXT].into_iter().zip(pager_names) {
        let mut bench = bench(report(25));
        drop(bench.update(Message::AnalyticsHistory(Input::Toggle)));
        drop(bench.update(Message::AnalyticsHistory(Input::Page(1))));
        let mut tree = Tree::empty();
        let (initial, _, _) = after_task(&bench, &mut tree, &renderer, operation::focus(focus));
        let ids = pager_names.map(|name| named(&initial, name).0);
        assert_eq!(initial.focus, named(&initial, focus_name).0);

        // A new chart report does not replace the frozen measurements. Its
        // availability caption must not replace the focused pager either.
        for truncated in [true, false] {
            let mut latest = (**bench.analytics.report.as_ref().unwrap()).clone();
            latest.collected_at_ms += 5_000;
            latest.truncated = truncated;
            bench.analytics.report = Some(Arc::new(latest));
            let (newer, bridge, window) = collect(&bench, &mut tree, &renderer);
            assert_eq!(pager_names.map(|name| named(&newer, name).0), ids);
            assert_eq!(newer.focus, named(&newer, focus_name).0);
            assert!(matches!(
                native_click(&bridge, window, ids[2]),
                Message::AnalyticsHistory(Input::Next)
            ));

            // Explicit Update changes both notice flags. Its normal action
            // intentionally returns focus to the retained page chooser.
            let update = named(&newer, "Update history").0;
            let task = bench.update(native_click(&bridge, window, update));
            let (updated, _, _) = after_task(&bench, &mut tree, &renderer, task);
            assert_eq!(pager_names.map(|name| named(&updated, name).0), ids);
            assert_eq!(updated.focus, ids[0]);
            assert_eq!(
                updated
                    .nodes
                    .iter()
                    .any(|(_, node)| node.label().is_some_and(
                        |label| label.starts_with("This snapshot reached the history limit.")
                    )),
                truncated
            );
            assert!(!updated.nodes.iter().any(|(_, node)| node.label()
                == Some("A newer report is available. The displayed snapshot has not changed.")));
            let (focused, _, _) = after_task(&bench, &mut tree, &renderer, operation::focus(focus));
            assert_eq!(focused.focus, named(&focused, focus_name).0);
        }
    }
}

#[test]
fn empty_single_truncated_and_large_histories_are_honest_and_bounded() {
    assert_eq!(overview(&report(0)).len(), 1);
    assert!(
        overview(&report(1))[0].contains("a change between measurements cannot be established")
    );
    assert_eq!(pages(0), 1);
    assert_eq!(pages(8), 1);
    assert_eq!(pages(9), 2);
    let renderer = super::super::super::destinations::layout_tests::renderer();
    let mut latest = (*report(1440)).clone();
    latest.truncated = true;
    let latest = Arc::new(latest);
    let mut bench = bench(latest.clone());
    drop(bench.update(Message::AnalyticsHistory(Input::Toggle)));
    drop(bench.update(Message::AnalyticsHistory(Input::Page(usize::MAX))));
    let (last, _, _) = collect(&bench, &mut Tree::empty(), &renderer);
    assert_eq!(bench.analytics.history.page, 179);
    assert_eq!(rows(&last).len(), PAGE_SIZE);
    // Bound the expanded history itself. Other sheet sections can add
    // controls without changing how many native history rows are published.
    let history = last
        .nodes
        .iter()
        .find(|(_, node)| {
            node.role() == accessibility::Role::Group && node.label() == Some("History snapshot")
        })
        .expect("expanded history has its own native group")
        .0;
    let nodes: HashMap<_, _> = last.nodes.iter().map(|(id, node)| (*id, node)).collect();
    let mut pending = vec![history];
    let mut history_nodes = std::collections::HashSet::new();
    while let Some(id) = pending.pop() {
        assert!(history_nodes.insert(id), "history nodes have one parent");
        pending.extend(
            nodes
                .get(&id)
                .expect("history child exists in the native tree")
                .children(),
        );
    }
    assert!(
        history_nodes.len() < 100,
        "1440 measurements must not create 1440 native rows"
    );
    assert!(last.nodes.iter().any(|(_, node)| {
        node.label()
            .is_some_and(|name| name.contains("Only the retained portion is available"))
    }));
    assert!(last.nodes.iter().any(|(_, node)| {
        node.label().is_some_and(|name| {
            name.contains(
                "missed percentage uses usable rate deltas; counts are observed bucket totals",
            )
        })
    }));
    assert_eq!(
        bench
            .analytics
            .history
            .input(Input::Update, Some(&report(0))),
        TOGGLE
    );
    let (empty, _, _) = collect(&bench, &mut Tree::empty(), &renderer);
    assert!(rows(&empty).is_empty());
    assert!(empty.nodes.iter().any(|(_, node)| {
        node.label()
            .is_some_and(|name| name.contains("one-second placeholder horizontal scale"))
    }));
    assert!(bench.analytics.history.row_ids.borrow().is_empty());
}

#[test]
fn compact_history_wraps_full_measurements_and_keeps_the_pager_inside_its_width() {
    use iced::advanced::widget::Operation;
    #[derive(Default)]
    struct TextBounds(Vec<(String, iced::Rectangle)>);
    impl Operation for TextBounds {
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
            operate(self);
        }
        fn text(
            &mut self,
            _: Option<&iced::advanced::widget::Id>,
            bounds: iced::Rectangle,
            value: &str,
        ) {
            self.0.push((value.into(), bounds));
        }
    }
    let renderer = super::super::super::destinations::layout_tests::renderer();
    let mut data = (*report(9)).clone();
    data.points[0].frames = 100_000;
    data.points[0].lost = 20_000;
    data.points[0].lost_percent = 0.25;
    data.cameras[0].name = "long-camera-name-".repeat(16);
    let data = Arc::new(data);
    let mut state = State::default();
    state.input(Input::Toggle, Some(&data));
    for scale in UiScale::ALL {
        for native_width in [360.0, 640.0] {
            let width = native_width / scale.factor() - 96.0;
            let mut view = state.view(&data, Palette::of(false));
            let mut tree = Tree::new(view.as_widget());
            let node = view.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, Size::new(width, f32::INFINITY)),
            );
            let mut text_bounds = TextBounds::default();
            view.as_widget_mut().operate(
                &mut tree,
                Layout::new(&node),
                &renderer,
                &mut text_bounds,
            );
            for (text, bounds) in text_bounds.0 {
                assert!(
                    bounds.x >= -0.01 && bounds.x + bounds.width <= width + 0.01,
                    "{scale:?}/{native_width}: {text} overflows {bounds:?} at {width}"
                );
            }
            let (snapshot, _, _) = accessibility::snapshot_widget(
                view.as_widget_mut(),
                &mut tree,
                Layout::new(&node),
                &renderer,
            );
            for name in [
                "History page",
                "Previous history page",
                "Next history page",
                "Hide history data",
                "Update history",
            ] {
                let (_, control) = named(&snapshot, name);
                let bounds = control.bounds().unwrap();
                assert!(
                    bounds.x0 >= 0.0 && bounds.x1 <= f64::from(width) + 0.01,
                    "{name} is clipped at {scale:?}/{native_width}: {bounds:?}"
                );
                assert!(bounds.height() > 20.0);
            }
            assert!(snapshot.nodes.iter().any(|(_, node)| node.label() == Some("Frame rate 2.00 frames per second · throughput 1.00 MiB per second · missed frames 0.25 percent")), "the reported usable-delta percentage must not be recomputed from baseline-inclusive counts");
            assert!(snapshot.nodes.iter().any(|(_, node)| node.label() == Some("Bucket totals: 100,000 acquired frames · 20,000 missed frames · 1,024 payload bytes")));
            assert!(snapshot.nodes.iter().any(|(_, node)| {
                node.label()
                    .is_some_and(|name| name.contains("initial counter baseline"))
            }));
        }
    }
}

#[test]
fn an_outdated_render_keeps_history_paired_with_the_displayed_chart_until_acceptance() {
    let renderer = super::super::super::destinations::layout_tests::renderer();
    let original = report(9);
    let mut bench = bench(original.clone());
    bench.appearance = Appearance::Light;
    let old = bench.analytics_chart_profile();
    let image = super::super::chart_fixture;
    bench
        .analytics
        .accept(Ok((original.clone(), image(), old)), old);
    drop(bench.update(Message::AnalyticsHistory(Input::Toggle)));
    let mut tree = Tree::empty();
    let (before, _, _) = collect(&bench, &mut tree, &renderer);
    let close_id = named(&before, "Close Session analytics").0;
    let toggle_id = named(&before, "Hide history data").0;
    let images = plot_ids(&before);
    let camera_id = named(&before, "Camera data volume: Workshop camera (sim:0)").0;
    let mut latest = (*original).clone();
    latest.collected_at_ms += 5_000;
    latest.points[0].fps = 99.0;
    let latest = Arc::new(latest);
    bench.appearance = Appearance::Dark;
    let current = bench.analytics_chart_profile();
    assert_ne!(old, current);
    bench
        .analytics
        .accept(Ok((latest.clone(), image(), old)), current);
    assert!(Arc::ptr_eq(
        bench.analytics.report.as_ref().unwrap(),
        &latest
    ));
    assert!(Arc::ptr_eq(
        bench.analytics.chart_report.as_ref().unwrap(),
        &original
    ));
    let (retained, _, _) = collect(&bench, &mut tree, &renderer);
    let caption = format!(
        "Chart is updating. Showing measurements from {}.",
        timestamp(original.collected_at_ms)
    );
    assert!(
        retained
            .nodes
            .iter()
            .any(|(_, node)| node.label() == Some(caption.as_str()))
    );
    assert!(
        named(&retained, "Update history").1.is_disabled(),
        "history can only update to the measurements actually shown in the chart"
    );
    assert!(!retained.nodes.iter().any(|(_, node)| {
        node.label()
            .is_some_and(|name| name.starts_with("Frame rate: first 99.00"))
    }));
    bench
        .analytics
        .accept(Ok((latest.clone(), image(), current)), current);
    let (accepted, bridge, window) = collect(&bench, &mut tree, &renderer);
    assert!(Arc::ptr_eq(
        bench.analytics.chart_report.as_ref().unwrap(),
        &latest
    ));
    assert!(
        !accepted
            .nodes
            .iter()
            .any(|(_, node)| node.label() == Some(caption.as_str()))
    );
    assert!(accepted.nodes.iter().any(|(_, node)| {
        node.label()
            .is_some_and(|name| name.starts_with("Frame rate: first 99.00"))
    }));
    assert!(!named(&accepted, "Update history").1.is_disabled());
    assert!(Arc::ptr_eq(
        bench.analytics.history.frozen.as_ref().unwrap(),
        &original
    ));
    for snapshot in [&retained, &accepted] {
        assert_eq!(named(snapshot, "Close Session analytics").0, close_id);
        assert_eq!(named(snapshot, "Hide history data").0, toggle_id);
        assert_eq!(plot_ids(snapshot), images);
        assert_eq!(
            named(snapshot, "Camera data volume: Workshop camera (sim:0)").0,
            camera_id
        );
        assert!(!snapshot.nodes.iter().any(|(_, node)| node.live().is_some()));
    }
    let update = named(&accepted, "Update history").0;
    drop(bench.update(native_click(&bridge, window, update)));
    assert!(Arc::ptr_eq(
        bench.analytics.history.frozen.as_ref().unwrap(),
        &latest
    ));
}
