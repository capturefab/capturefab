//! Session analytics use the same DuckDB queries and Plotters chart as HTTP.
//! Queries and chart construction run off the UI thread, even while capturing.
use super::*;
use crate::analytics::AnalyticsSnapshot;
use iced::widget::column;
mod chart;
mod history;
pub(super) use history::Input as HistoryInput;

pub(super) fn measurement_timestamp(value: u64) -> String {
    history::timestamp(value)
}

type Report = std::result::Result<(Arc<AnalyticsSnapshot>, chart::Graphic, chart::Profile), String>;
const REFRESH: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Tab {
    #[default]
    Acquisition,
    Measurements,
}

#[derive(Default)]
pub(super) struct State {
    pub open: bool,
    pub scrolled: bool,
    pub tab: Tab,
    pending: Option<Receiver<Report>>,
    refreshed: Option<Instant>,
    report: Option<Arc<AnalyticsSnapshot>>,
    chart: Option<chart::Graphic>,
    chart_view: chart::ViewState,
    chart_report: Option<Arc<AnalyticsSnapshot>>,
    chart_profile: Option<chart::Profile>,
    requested_profile: Option<chart::Profile>,
    error: Option<String>,
    history: history::State,
}

impl State {
    fn accept(&mut self, result: Report, current: chart::Profile) {
        match result {
            Ok((report, chart, profile)) => {
                self.report = Some(report);
                // A resize or appearance change may finish before the worker.
                // Retain its measurements, then render them for the current view.
                if profile == current {
                    self.chart = Some(chart);
                    self.chart_profile = Some(profile);
                    self.chart_report = self.report.clone();
                }
                self.error = None;
            }
            Err(error) => self.error = Some(error),
        }
    }
}

impl Workbench {
    pub(super) fn analytics_tab(&mut self, tab: Tab) -> Task<Message> {
        if self.analytics.tab == tab || !self.analytics.open {
            return Task::none();
        }
        self.analytics.tab = tab;
        self.analytics.scrolled = false;
        // Only the visible tab refreshes, so the one shown now may be stale.
        match tab {
            Tab::Acquisition => self.analytics.refreshed = None,
            Tab::Measurements => self.video_analytics.expire(),
        }
        self.tick_analytics();
        self.tick_video_analytics();
        operation::snap_to(SHEET_BODY, operation::RelativeOffset::START)
    }

    fn analytics_sheet_width(&self) -> f32 {
        (self.width - 48.0).clamp(0.0, 1060.0)
    }

    pub(super) fn analytics_svg_options(&self) -> crate::analytics::SvgOptions {
        crate::analytics::SvgOptions::interface(
            (self.analytics_sheet_width() - 48.0).floor() as u32,
            chart::colors(Palette::of(self.dark())),
        )
    }

    fn analytics_chart_profile(&self) -> chart::Profile {
        chart::profile(
            self.analytics_sheet_width() - 48.0,
            Palette::of(self.dark()),
        )
    }

    pub(super) fn analytics_staged(&self) -> bool {
        !self.analytics.open
            || self.analytics.report.as_ref().is_some_and(|report| {
                self.snapshot.cameras.is_empty()
                    || report.points.iter().any(|point| point.fps > 0.0)
            })
    }

    pub(super) fn show_analytics(&mut self, open: bool) -> Task<Message> {
        if open && !self.analytics.open && self.sheet_open() {
            return Task::none();
        }
        let opening = open && !self.analytics.open;
        self.analytics.open = open;
        if opening {
            self.analytics.history.reset();
            self.analytics.scrolled = false;
            self.analytics.refreshed = None;
            self.tick_analytics();
            Task::batch([
                operation::focus("sheet-close"),
                operation::snap_to(SHEET_BODY, operation::RelativeOffset::START),
            ])
        } else {
            Task::none()
        }
    }

    pub(super) fn analytics_history(&mut self, input: HistoryInput) -> Task<Message> {
        if !self.analytics.open {
            return Task::none();
        }
        let report = self
            .analytics
            .chart_report
            .as_ref()
            .or(self.analytics.report.as_ref());
        let focus = self.analytics.history.input(input, report);
        accessibility::focus_in_window(focus, self.main_window)
    }

    pub(super) fn tick_analytics(&mut self) {
        let profile = self.analytics_chart_profile();
        if let Some(pending) = &self.analytics.pending {
            match pending.try_recv() {
                Ok(result) => {
                    self.analytics.pending = None;
                    self.analytics.accept(result, profile);
                }
                Err(TryRecvError::Disconnected) => {
                    self.analytics.pending = None;
                    self.analytics.error =
                        Some("Analytics worker stopped. Refreshing shortly.".into());
                }
                Err(TryRecvError::Empty) => {}
            }
        }
        if !self.analytics.open
            || self.analytics.tab != Tab::Acquisition
            || self.analytics.pending.is_some()
        {
            return;
        }
        let query_due = self
            .analytics
            .refreshed
            .is_none_or(|at| self.now.saturating_duration_since(at) >= REFRESH);
        let render_due =
            self.analytics.requested_profile != Some(profile) && self.analytics.report.is_some();
        if !query_due && !render_due {
            return;
        }
        let cached = if query_due {
            self.analytics.refreshed = Some(self.now);
            None
        } else {
            self.analytics.report.clone()
        };
        self.analytics.requested_profile = Some(profile);
        let handle = self.handle.clone();
        let (sender, receiver) = mpsc::channel();
        match std::thread::Builder::new()
            .name("capturefab-analytics-view".into())
            .spawn(move || {
                let report = (|| -> anyhow::Result<_> {
                    let report = match cached {
                        Some(report) => report,
                        None => Arc::new(handle.analytics().snapshot()?),
                    };
                    let chart = chart::render(&report, profile)?;
                    Ok((report, chart, profile))
                })()
                .map_err(|error| format!("{error:#}"));
                let _ = sender.send(report);
            }) {
            Ok(_) => self.analytics.pending = Some(receiver),
            Err(error) => self.analytics.error = Some(format!("Could not load analytics: {error}")),
        }
    }

    pub(super) fn analytics_sheet(&self, p: &'static Palette, height: f32) -> Element<'_, Message> {
        let width = self.analytics_sheet_width();
        let tab = self.analytics.tab;
        let selected = usize::from(tab == Tab::Measurements);
        let tabs = panel_tabs(
            "Session analytics views",
            vec![
                (
                    "Acquisition",
                    Some(Message::AnalyticsTab(Tab::Acquisition)),
                    "Frame rate, payload, missed frames and session history".into(),
                ),
                (
                    "Prompt measurements",
                    Some(Message::AnalyticsTab(Tab::Measurements)),
                    "Configure measurements and review collected values".into(),
                ),
            ],
            selected,
            selected as f32,
        );
        let body = match tab {
            Tab::Acquisition => self.acquisition_content(p),
            Tab::Measurements => self.video_analytics_content(p),
        };
        sheet_frame("Session analytics", Message::Analytics(false), body, p)
            .mark(Icon::Activity)
            .controls(tabs)
            .size(width, height)
            .scrolled(self.analytics.scrolled)
            .on_scroll(Message::AnalyticsScrolled)
            .into()
    }

    fn acquisition_content(&self, p: &'static Palette) -> Element<'_, Message> {
        let mut introduction = column![
            text("All cameras · current capture session").size(style::BODY).font(style::MEDIUM),
            text("History covers at most the last 24 hours. Session totals include earlier activity and reconnects. Updates every 5 seconds.")
                .size(style::SMALL).color(p.secondary),
        ].spacing(14);
        if let Some(error) = &self.analytics.error {
            introduction =
                introduction.push(field_error(format!("Analytics unavailable: {error}"), p));
        }
        // Keep asynchronous errors inside the introduction so existing report
        // widgets retain their native identities when a refresh fails.
        let mut body = column![introduction,].spacing(14);
        if let Some(report) = &self.analytics.report {
            let attempted = report.totals.frames.saturating_add(report.totals.lost);
            let loss = if attempted == 0 {
                0.0
            } else {
                report.totals.lost as f64 / attempted as f64 * 100.0
            };
            let elapsed = report.collected_at_ms.saturating_sub(report.started_at_ms) / 1000;
            let mut summary = column![accessibility::group(
                container(
                    column![
                        text(format!(
                            "{} frames acquired · {:.1} MiB payload",
                            grouped(report.totals.frames),
                            report.totals.bytes as f64 / 1_048_576.0
                        ))
                        .size(style::BODY)
                        .font(style::MEDIUM),
                        text(format!(
                            "{} missed frames ({loss:.2}%) · session {:02}:{:02}:{:02}",
                            grouped(report.totals.lost),
                            elapsed / 3600,
                            elapsed / 60 % 60,
                            elapsed % 60
                        ))
                        .size(style::SMALL)
                        .color(p.secondary),
                    ]
                    .spacing(7)
                )
                .padding(14)
                .width(Fill)
                .style(style::code)
                .into(),
                "Session totals".into(),
                accessibility::Role::Group,
            )]
            .spacing(14);
            if report.totals.frames == 0 {
                summary = summary.push(
                    text("Start a stream or save a capture to see acquisition activity here.")
                        .size(style::BODY)
                        .color(p.secondary),
                );
            }
            if report.truncated {
                summary = summary.push(text("The history limit was reached. The chart shows the retained portion of this session.").size(style::SMALL).color(p.ink(p.warn)));
            }
            if self.analytics.error.is_some() {
                summary = summary.push(
                    text("Showing the last successful update.")
                        .size(style::SMALL)
                        .color(p.secondary),
                );
            }
            body = body.push(summary);
            let chart_report = self.analytics.chart_report.as_ref().unwrap_or(report);
            let context: Element<'_, Message> =
                if self.analytics.chart.is_some() && !Arc::ptr_eq(chart_report, report) {
                    container(
                        text(format!(
                            "Chart is updating. Showing measurements from {}.",
                            history::timestamp(chart_report.collected_at_ms)
                        ))
                        .size(style::SMALL)
                        .color(p.secondary)
                        .width(Fill)
                        .wrapping(text::Wrapping::WordOrGlyph),
                    )
                    .padding(iced::Padding {
                        bottom: 14.0,
                        ..iced::Padding::ZERO
                    })
                    .into()
                } else {
                    space().into()
                };
            let image: Element<'_, Message> = if let Some(chart) = &self.analytics.chart {
                let profile = self
                    .analytics
                    .chart_profile
                    .unwrap_or_else(|| self.analytics_chart_profile());
                chart::view(
                    chart_report,
                    chart,
                    profile,
                    self.analytics_chart_profile(),
                    p,
                    &self.analytics.chart_view,
                )
            } else {
                space().into()
            };
            // Fixed slots preserve the chart and disclosure widget state while
            // a stale render is retained or its explanatory caption changes.
            body = body.push(
                column![
                    container(context),
                    container(image),
                    container(self.analytics.history.view(chart_report, p)).padding(
                        iced::Padding {
                            top: 14.0,
                            ..iced::Padding::ZERO
                        }
                    ),
                ]
                .width(Fill),
            );
            for camera in &report.cameras {
                body = body.push(
                    text(format!(
                        "{} ({}) · {:.1} fps · {:.2} MiB/s · {:.2}% missed{}",
                        camera.name,
                        camera.id,
                        camera.fps,
                        camera.bytes_per_second / 1_048_576.0,
                        camera.lost_percent,
                        if camera.stale {
                            " · waiting for worker"
                        } else if camera.streaming {
                            " · streaming"
                        } else {
                            " · idle"
                        }
                    ))
                    .size(style::SMALL)
                    .color(p.secondary),
                );
            }
        } else if self.analytics.error.is_none() {
            body = body.push(accessibility::label(
                text("Loading session analytics…")
                    .size(style::BODY)
                    .color(p.secondary)
                    .into(),
                "Loading session analytics…".into(),
                accessibility::Role::Status,
            ));
        }
        body = body.push(text("Missed frames were lost on the camera link or in the camera's worker. Preview frames the workbench skipped are not counted; captures and recordings still received them. Payload is acquired image data before encoding.").size(style::SMALL).color(p.secondary));
        body.into()
    }
}

#[cfg(test)]
fn chart_fixture() -> chart::Graphic {
    chart::Graphic {
        plots: std::array::from_fn(|_| {
            iced::widget::svg::Handle::from_memory(
                br#"<svg xmlns="http://www.w3.org/2000/svg" width="640" height="360"></svg>"#,
            )
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced::advanced::{Layout, layout, widget::Tree};

    #[test]
    fn analytics_tabs_remain_visible_when_scrolled_and_preserve_measurement_drafts() {
        use super::super::destinations::layout_tests::{apply, render_artifact_scaled};
        use iced::futures::StreamExt;
        let mut renderer = super::super::destinations::layout_tests::renderer();
        for scale in [1.0, 1.15, 1.3] {
            let size = Size::new(900.0 / scale, 620.0 / scale);
            let height = size.height - 80.0;
            let mut bench =
                Workbench::new(SessionHandle::new(), "analytics-tabs".into(), true, None);
            bench.width = size.width;
            bench.analytics.open = true;
            bench.analytics.report = Some(Arc::new(report()));
            bench.analytics.chart = Some(chart_fixture());
            bench.video_analytics.preferences.measurements[0].label = "Retained draft".into();
            let mut tree = Tree::empty();
            let mut previous = None;
            for tab in [Tab::Acquisition, Tab::Measurements, Tab::Acquisition] {
                let _ = bench.analytics_tab(tab);
                let mut view = bench.analytics_sheet(Palette::of(false), height);
                tree.diff(view.as_widget());
                let node = view.as_widget_mut().layout(
                    &mut tree,
                    &renderer,
                    &layout::Limits::new(Size::ZERO, size),
                );
                let snapshot = accessibility::snapshot_widget(
                    view.as_widget_mut(),
                    &mut tree,
                    Layout::new(&node),
                    &renderer,
                )
                .0;
                let tabs: Vec<_> = snapshot
                    .nodes
                    .iter()
                    .filter(|(_, node)| node.role() == accessibility::Role::Tab)
                    .collect();
                assert_eq!(tabs.len(), 2);
                let identity: Vec<_> = tabs
                    .iter()
                    .map(|(id, node)| (*id, node.bounds().unwrap()))
                    .collect();
                if let Some(previous) = &previous {
                    assert_eq!(
                        &identity, previous,
                        "tab controls retain native identities and geometry"
                    );
                }
                previous = Some(identity.clone());
                for (index, (_, node)) in tabs.iter().enumerate() {
                    assert_eq!(
                        node.is_selected(),
                        Some(index == usize::from(tab == Tab::Measurements))
                    );
                    let bounds = node.bounds().unwrap();
                    assert!(bounds.y0 >= 0.0 && bounds.y1 < f64::from(height));
                    assert!(bounds.x0 >= 0.0 && bounds.x1 <= f64::from(size.width));
                }
                if tab == Tab::Acquisition {
                    let totals = snapshot
                        .nodes
                        .iter()
                        .find(|(_, node)| {
                            node.label() == Some("989 frames acquired · 1.0 MiB payload")
                        })
                        .unwrap()
                        .1
                        .bounds()
                        .unwrap();
                    assert!(
                        totals.y0 >= identity[0].1.y1 && totals.y1 <= f64::from(height),
                        "session totals are immediately visible"
                    );
                }
                apply(
                    &mut view,
                    &mut tree,
                    &node,
                    &renderer,
                    iced::advanced::widget::operation::scrollable::snap_to(
                        SHEET_BODY.into(),
                        operation::RelativeOffset::END.into(),
                    ),
                );
                let (scrolled, bridge, window) = accessibility::snapshot_widget(
                    view.as_widget_mut(),
                    &mut tree,
                    Layout::new(&node),
                    &renderer,
                );
                for (id, bounds) in &identity {
                    assert_eq!(
                        scrolled
                            .nodes
                            .iter()
                            .find(|(node_id, _)| node_id == id)
                            .unwrap()
                            .1
                            .bounds(),
                        Some(*bounds),
                        "navigation stays pinned when content scrolls"
                    );
                }
                render_artifact_scaled(
                    &format!("analytics-{tab:?}-{scale}"),
                    &view,
                    &tree,
                    &node,
                    &mut renderer,
                    size,
                    scale,
                );
                let target = tabs[usize::from(tab == Tab::Acquisition)].0;
                let task = bridge.dispatch(accessibility::Request {
                    window,
                    replay: None,
                    action: accesskit::ActionRequest {
                        action: accesskit::Action::Click,
                        target,
                        data: None,
                    },
                });
                let mut stream = iced_runtime::task::into_stream(task).unwrap();
                let Some(iced_runtime::Action::Output(Message::AnalyticsTab(next))) =
                    iced::futures::executor::block_on(stream.next())
                else {
                    panic!("tabs route through the native command path")
                };
                drop(view);
                let _ = bench.handle_message(Message::AnalyticsTab(next));
                assert_eq!(
                    bench.video_analytics.preferences.measurements[0].label,
                    "Retained draft"
                );
            }
        }
    }

    fn report() -> AnalyticsSnapshot {
        AnalyticsSnapshot {
            retention_seconds: 86_400,
            sample_interval_seconds: 1,
            started_at_ms: 1_000,
            collected_at_ms: 66_000,
            samples: 65,
            truncated: false,
            totals: crate::analytics::AnalyticsTotals {
                frames: 989,
                dropped: 11,
                bytes: 1_048_576,
                lost: 11,
                preview_skipped: 0,
            },
            cameras: vec![crate::analytics::CameraAnalytics {
                id: "sim:0".into(),
                name: "Workshop camera".into(),
                frames: 989,
                dropped: 11,
                bytes: 1_048_576,
                fps: 15.2,
                bytes_per_second: 1_048_576.0,
                drop_percent: 1.1,
                streaming: true,
                stale: false,
                lost: 11,
                preview_skipped: 0,
                lost_percent: 1.1,
            }],
            points: vec![],
        }
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

    #[test]
    fn analytics_totals_keep_visible_reading_order_and_native_identity_during_refresh_errors() {
        use iced::futures::StreamExt;
        let renderer = super::super::destinations::layout_tests::renderer();
        let mut bench = Workbench::new(SessionHandle::new(), "analytics-tree".into(), true, None);
        bench.width = 900.0;
        bench.analytics.report = Some(Arc::new(report()));
        bench.analytics.chart = Some(chart_fixture());
        let mut tree = Tree::empty();
        let mut previous = None;
        for error in [None, Some("Session database could not be read"), None] {
            bench.analytics.error = error.map(str::to_owned);
            let (snapshot, bridge, window) = collect(&bench, &mut tree, &renderer);
            let nodes: HashMap<_, _> = snapshot
                .nodes
                .iter()
                .map(|(id, node)| (*id, node))
                .collect();
            let (totals_id, totals) = snapshot
                .nodes
                .iter()
                .find(|(_, node)| {
                    node.role() == accessibility::Role::Group
                        && node.label() == Some("Session totals")
                })
                .expect("the totals have a real semantic parent");
            let labels: Vec<_> = totals
                .children()
                .iter()
                .map(|id| nodes[id].label().unwrap())
                .collect();
            assert_eq!(
                labels,
                [
                    "989 frames acquired · 1.0 MiB payload",
                    "11 missed frames (1.10%) · session 00:01:05",
                ]
            );
            for child in totals.children() {
                assert_eq!(
                    snapshot
                        .nodes
                        .iter()
                        .filter(|(_, node)| node.children().contains(child))
                        .count(),
                    1
                );
                assert_eq!(
                    snapshot
                        .nodes
                        .iter()
                        .filter(|(_, node)| node.label() == nodes[child].label())
                        .count(),
                    1
                );
            }
            assert!(!totals.supports_action(accesskit::Action::Focus));
            assert!(!totals.supports_action(accesskit::Action::Click));
            let image_ids: Vec<_> = snapshot
                .nodes
                .iter()
                .filter(|(_, node)| node.role() == accessibility::Role::Image)
                .map(|(id, _)| *id)
                .collect();
            assert_eq!(image_ids.len(), 3);
            let camera_id = snapshot
                .nodes
                .iter()
                .find(|(_, node)| {
                    node.label() == Some("Camera data volume: Workshop camera (sim:0)")
                })
                .unwrap()
                .0;
            let (close_id, close) = snapshot
                .nodes
                .iter()
                .find(|(_, node)| node.label() == Some("Close Session analytics"))
                .unwrap();
            assert!(close.supports_action(accesskit::Action::Click));
            if let Some(previous) = previous {
                assert_eq!(
                    (*totals_id, image_ids.clone(), camera_id, *close_id),
                    previous,
                    "a failed refresh preserves existing report and Close identities"
                );
            }
            previous = Some((*totals_id, image_ids, camera_id, *close_id));
            let alerts: Vec<_> = snapshot
                .nodes
                .iter()
                .filter(|(_, node)| node.role() == accessibility::Role::Alert)
                .collect();
            assert_eq!(alerts.len(), usize::from(error.is_some()));
            if error.is_some() {
                assert_eq!(
                    alerts[0].1.value(),
                    Some("Analytics unavailable: Session database could not be read")
                );
            }
            assert_eq!(snapshot.nodes.iter().filter(|(_, node)| node.label() == Some("Workshop camera (sim:0) · 15.2 fps · 1.00 MiB/s · 1.10% missed · streaming")).count(), 1);
            let mut actions =
                iced_runtime::task::into_stream(bridge.dispatch(accessibility::Request {
                    window,
                    replay: None,
                    action: accesskit::ActionRequest {
                        action: accesskit::Action::Click,
                        target: *close_id,
                        data: None,
                    },
                }))
                .unwrap();
            assert!(matches!(
                iced::futures::executor::block_on(actions.next()),
                Some(iced_runtime::Action::Output(Message::Analytics(false)))
            ));
            assert!(iced::futures::executor::block_on(actions.next()).is_none());
        }
    }

    #[test]
    fn analytics_loading_is_one_native_live_value_and_failed_loading_keeps_close_available() {
        let renderer = super::super::destinations::layout_tests::renderer();
        let mut bench =
            Workbench::new(SessionHandle::new(), "analytics-loading".into(), true, None);
        bench.width = 900.0;
        let mut tree = Tree::empty();
        let (loading, _, _) = collect(&bench, &mut tree, &renderer);
        let status: Vec<_> = loading
            .nodes
            .iter()
            .filter(|(_, node)| node.label() == Some("Loading session analytics…"))
            .collect();
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].1.role(), accessibility::Role::Status);
        assert_eq!(status[0].1.value(), Some("Loading session analytics…"));
        assert_eq!(status[0].1.live(), Some(accesskit::Live::Polite));
        let close_id = loading
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some("Close Session analytics"))
            .unwrap()
            .0;
        bench.analytics.error = Some("No session history is available".into());
        let (failed, _, _) = collect(&bench, &mut tree, &renderer);
        assert!(
            !failed
                .nodes
                .iter()
                .any(|(_, node)| node.role() == accessibility::Role::Status)
        );
        let (after_id, close) = failed
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some("Close Session analytics"))
            .unwrap();
        assert_eq!(*after_id, close_id);
        assert!(!close.is_disabled());
        assert!(close.supports_action(accesskit::Action::Click));
    }

    #[test]
    fn delayed_chart_reply_retains_data_without_installing_an_outdated_appearance() {
        let mut bench = Workbench::new(SessionHandle::new(), "chart-generation".into(), true, None);
        bench.width = 900.0;
        bench.appearance = Appearance::Light;
        let old_profile = bench.analytics_chart_profile();
        bench.appearance = Appearance::Dark;
        let current = bench.analytics_chart_profile();
        assert_ne!(old_profile, current);
        let report = Arc::new(report());
        let (sender, receiver) = mpsc::channel();
        sender
            .send(Ok((report.clone(), chart_fixture(), old_profile)))
            .unwrap();
        bench.analytics.pending = Some(receiver);
        bench.analytics.requested_profile = Some(old_profile);
        // A closed sheet still receives the result, without starting more work.
        bench.tick_analytics();
        assert!(Arc::ptr_eq(
            bench.analytics.report.as_ref().unwrap(),
            &report
        ));
        assert!(bench.analytics.chart.is_none());
        assert!(bench.analytics.chart_profile.is_none());
        assert!(bench.analytics.pending.is_none());
    }

    #[test]
    fn acquisition_data_refreshes_only_while_its_tab_shows() {
        let mut bench = Workbench::new(SessionHandle::new(), "visible-tab".into(), true, None);
        bench.width = 900.0;
        bench.analytics.tab = Tab::Measurements;
        drop(bench.show_analytics(true));
        assert!(
            bench.analytics.pending.is_none(),
            "hidden behind measurements"
        );
        drop(bench.analytics_tab(Tab::Acquisition));
        let reply = bench.analytics.pending.take().expect("loads when shown");
        bench.analytics.accept(
            reply.recv_timeout(Duration::from_secs(10)).unwrap(),
            bench.analytics_chart_profile(),
        );
        assert!(bench.analytics.report.is_some());
        drop(bench.analytics_tab(Tab::Measurements));
        bench.analytics.refreshed = None;
        bench.tick_analytics();
        assert!(bench.analytics.pending.is_none(), "due, but not shown");
    }

    #[test]
    fn chart_resize_renders_cached_measurements_off_thread_before_the_next_query() {
        let mut bench = Workbench::new(SessionHandle::new(), "chart-resize".into(), true, None);
        bench.width = 1200.0;
        bench.appearance = Appearance::Light;
        let old_profile = bench.analytics_chart_profile();
        let report = Arc::new(report());
        bench.analytics.open = true;
        bench.analytics.report = Some(report.clone());
        bench.analytics.refreshed = Some(bench.now);
        bench.analytics.requested_profile = Some(old_profile);
        bench.width = 640.0 / UiScale::Larger.factor();
        let current = bench.analytics_chart_profile();
        assert_ne!(old_profile, current);
        bench.tick_analytics();
        let reply = bench
            .analytics
            .pending
            .take()
            .expect("resize starts a render task")
            .recv_timeout(Duration::from_secs(10))
            .expect("background chart render completes")
            .expect("cached chart is renderable");
        assert!(
            Arc::ptr_eq(&reply.0, &report),
            "a resize keeps the same measurements rather than querying the session"
        );
        assert_eq!(reply.2, current);
        assert_eq!(bench.analytics.refreshed, Some(bench.now));
        bench.analytics.accept(Ok(reply), current);
        assert_eq!(bench.analytics.chart_profile, Some(current));
        assert!(bench.analytics.chart.is_some());
        bench.tick_analytics();
        assert!(
            bench.analytics.pending.is_none(),
            "unchanged size and appearance do not repeat rendering"
        );
    }

    #[test]
    fn fitted_analytics_sheet_keeps_chart_at_intrinsic_width_at_every_interface_size() {
        let renderer = super::super::destinations::layout_tests::renderer();
        for scale in UiScale::ALL {
            for native_width in [360.0, 640.0, 900.0] {
                let mut bench =
                    Workbench::new(SessionHandle::new(), "chart-fit".into(), true, None);
                bench.width = native_width / scale.factor();
                bench.ui_scale = scale;
                bench.analytics.open = true;
                let base = bench.analytics_chart_profile();
                let report = Arc::new(report());
                let profile = base;
                let graphic = chart::render(&report, profile).unwrap();
                bench.analytics.accept(Ok((report, graphic, profile)), base);
                let mut view = bench.window_view(bench.main_window);
                let mut tree = Tree::new(view.as_widget());
                let size = Size::new(bench.width, 620.0 / scale.factor());
                let layout = view.as_widget_mut().layout(
                    &mut tree,
                    &renderer,
                    &layout::Limits::new(Size::ZERO, size),
                );
                let (snapshot, _, _) = accessibility::snapshot_widget(
                    view.as_widget_mut(),
                    &mut tree,
                    Layout::new(&layout),
                    &renderer,
                );
                let image = snapshot
                    .nodes
                    .iter()
                    .find(|(_, node)| node.role() == accessibility::Role::Image)
                    .expect("actual chart is in the sheet")
                    .1
                    .bounds()
                    .unwrap();
                assert!(
                    (image.width() - f64::from(profile.panel_width)).abs() < 0.5,
                    "native width {native_width}, {scale:?}: chart shrank from {} to {} logical units",
                    profile.panel_width,
                    image.width()
                );
                let close = snapshot
                    .nodes
                    .iter()
                    .find(|(_, node)| node.label() == Some("Close Session analytics"))
                    .unwrap()
                    .1
                    .bounds()
                    .unwrap();
                assert!(
                    close.x0 >= 0.0
                        && close.x1 <= f64::from(size.width) + 0.5
                        && close.y0 >= 0.0
                        && close.y1 <= f64::from(size.height) + 0.5
                );
            }
        }
    }

    #[test]
    fn changed_camera_count_keeps_the_base_profile_without_repeating_a_render() {
        let mut bench = Workbench::new(
            SessionHandle::new(),
            "chart-camera-count".into(),
            true,
            None,
        );
        bench.width = 480.0;
        bench.analytics.open = true;
        let base = bench.analytics_chart_profile();
        let first = Arc::new(report());
        let mut expanded = (*first).clone();
        let template = expanded.cameras[0].clone();
        for index in 1..6 {
            let mut camera = template.clone();
            camera.id = format!("sim:{index}");
            expanded.cameras.push(camera);
        }
        let expanded = Arc::new(expanded);
        for report in [first, expanded] {
            let profile = base;
            bench
                .analytics
                .accept(Ok((report.clone(), chart_fixture(), profile)), base);
            assert_eq!(bench.analytics.chart_profile, Some(profile));
            assert!(Arc::ptr_eq(
                bench.analytics.report.as_ref().unwrap(),
                &report
            ));
            bench.analytics.refreshed = Some(bench.now);
            bench.analytics.requested_profile = Some(base);
            bench.tick_analytics();
            assert!(
                bench.analytics.pending.is_none(),
                "report-derived height must not cause a repeated render against the base profile"
            );
        }
    }
}
