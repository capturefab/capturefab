//! A bounded, non-live view of the measurements drawn by the session chart.
use super::*;
use crate::analytics::AnalyticsPoint;
use iced::widget::column;
use std::cell::RefCell;

const PAGE_SIZE: usize = 8;
pub(super) const TOGGLE: &str = "analytics-history-toggle";
const PAGE: &str = "analytics-history-page";
const PREVIOUS: &str = "analytics-history-previous";
const NEXT: &str = "analytics-history-next";

#[derive(Debug, Clone, Copy)]
pub(in crate::gui) enum Input {
    Toggle,
    Previous,
    Next,
    Page(usize),
    Update,
}

pub(super) struct State {
    frozen: Option<Arc<AnalyticsSnapshot>>,
    page: usize,
    overview_id: accessibility::NodeId,
    axes_id: accessibility::NodeId,
    snapshot_id: accessibility::NodeId,
    row_ids: RefCell<HashMap<u64, accessibility::NodeId>>,
    camera_ids: RefCell<HashMap<String, accessibility::NodeId>>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            frozen: None,
            page: 0,
            overview_id: accessibility::new_node_id(),
            axes_id: accessibility::new_node_id(),
            snapshot_id: accessibility::new_node_id(),
            row_ids: RefCell::default(),
            camera_ids: RefCell::default(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Page {
    index: usize,
    count: usize,
}
impl std::fmt::Display for Page {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Page {} of {}", self.index + 1, self.count)
    }
}

impl State {
    pub(super) fn reset(&mut self) {
        self.frozen = None;
        self.page = 0;
        self.row_ids.get_mut().clear();
    }

    pub(super) fn input(
        &mut self,
        input: Input,
        current: Option<&Arc<AnalyticsSnapshot>>,
    ) -> &'static str {
        match input {
            Input::Toggle => {
                if self.frozen.is_some() {
                    self.reset();
                } else {
                    self.frozen = current.cloned();
                    self.page = 0;
                }
                return TOGGLE;
            }
            Input::Update => {
                if self.frozen.is_none() {
                    return TOGGLE;
                }
                if let Some(current) = current {
                    self.frozen = Some(current.clone());
                    self.row_ids
                        .get_mut()
                        .retain(|time, _| current.points.iter().any(|p| p.timestamp_ms == *time));
                }
            }
            Input::Previous => self.page = self.page.saturating_sub(1),
            Input::Next => self.page = self.page.saturating_add(1),
            Input::Page(page) => self.page = page,
        }
        let Some(report) = &self.frozen else {
            return TOGGLE;
        };
        if report.points.is_empty() {
            self.page = 0;
            return TOGGLE;
        }
        let last = pages(report.points.len()).saturating_sub(1);
        self.page = self.page.min(last);
        match input {
            Input::Previous if self.page > 0 => PREVIOUS,
            Input::Next if self.page < last => NEXT,
            _ => PAGE,
        }
    }

    pub(super) fn view<'a>(
        &'a self,
        current: &'a Arc<AnalyticsSnapshot>,
        p: &'static Palette,
    ) -> Element<'a, Message> {
        let overview = overview(current);
        let mut measurements = column![heading("Chart measurements", p)].spacing(6);
        for line in overview {
            measurements = measurements.push(caption(line, p));
        }
        let mut content = column![accessibility::group_with_id(
            measurements.into(),
            "Chart measurements".into(),
            self.overview_id,
        )]
        .spacing(12)
        .width(Fill);
        // Match the shared disclosure's role and expanded state, with an
        // explicit focus ID so hiding the rows retains keyboard focus here.
        let expanded = self.frozen.is_some();
        content = content.push(
            button(
                row![
                    icon(
                        if expanded {
                            Icon::ChevronDown
                        } else {
                            Icon::ChevronRight
                        },
                        11.0,
                        p.secondary,
                    ),
                    text(if expanded {
                        "Hide history data"
                    } else {
                        "Show history data"
                    })
                    .size(style::BODY)
                    .font(style::MEDIUM)
                    .width(Fill),
                ]
                .spacing(6)
                .align_y(Alignment::Center),
            )
            .id(TOGGLE)
            .name(if expanded {
                "Hide history data"
            } else {
                "Show history data"
            })
            .role(accessibility::Role::DisclosureTriangle)
            .expanded(expanded)
            .width(Fill)
            .padding([3, 0])
            .style(style::plain)
            .on_press(Message::AnalyticsHistory(Input::Toggle)),
        );
        if let Some(report) = &self.frozen {
            let count = pages(report.points.len());
            let page = self.page.min(count.saturating_sub(1));
            let start = page * PAGE_SIZE;
            let end = (start + PAGE_SIZE).min(report.points.len());
            let updated = current.collected_at_ms != report.collected_at_ms;
            // Keep transient notices inside a persistent context slot. Adding
            // them as details siblings would replace the pager's widget state,
            // native IDs, and focus when a newer report arrives.
            let mut context = column![self.axes(report, p)].spacing(10).width(Fill);
            if updated {
                context = context.push(caption(
                    "A newer report is available. The displayed snapshot has not changed.".into(),
                    p,
                ));
            }
            if report.truncated {
                context = context.push(caption("This snapshot reached the history limit. Only the retained portion is available; it does not cover the whole session.".into(), p));
            }
            let mut details = column![
                heading("History snapshot", p),
                caption(format!(
                    "Snapshot as of {}. These rows stay fixed while the chart and its measurement summary update. Use Update history to load the latest report.",
                    timestamp(report.collected_at_ms),
                ), p),
                button(text("Update history").size(style::SMALL))
                    .name("Update history")
                    .padding([5, 8])
                    .style(style::link)
                    .on_press_maybe(updated.then_some(Message::AnalyticsHistory(Input::Update))),
                context,
            ].spacing(10).width(Fill);
            if report.points.is_empty() {
                details = details.push(caption(
                    "No retained history points are available in this snapshot.".into(),
                    p,
                ));
            } else {
                let choices: Vec<_> = (0..count).map(|index| Page { index, count }).collect();
                details = details.push(
                    pick_list(choices, Some(Page { index: page, count }), |page| {
                        Message::AnalyticsHistory(Input::Page(page.index))
                    })
                    .id(PAGE)
                    .name("History page")
                    .text_size(style::SMALL)
                    .width(Fill),
                );
                details = details.push(
                    row![
                        button(text("Previous").size(style::SMALL))
                            .id(PREVIOUS)
                            .name("Previous history page")
                            .padding([5, 8])
                            .style(style::plain)
                            .on_press_maybe(
                                (page > 0).then_some(Message::AnalyticsHistory(Input::Previous))
                            ),
                        space::horizontal(),
                        button(text("Next").size(style::SMALL))
                            .id(NEXT)
                            .name("Next history page")
                            .padding([5, 8])
                            .style(style::plain)
                            .on_press_maybe(
                                (page + 1 < count)
                                    .then_some(Message::AnalyticsHistory(Input::Next))
                            ),
                    ]
                    .spacing(6)
                    .align_y(Alignment::Center),
                );
                details = details.push(caption(format!("Measurements {}–{} of {}, oldest first. Frame/byte rates are bucket averages; missed percentage uses usable rate deltas; counts are observed bucket totals.", start + 1, end, report.points.len()), p));
                let first = report.points[0].timestamp_ms;
                let mut ids = self.row_ids.borrow_mut();
                let rows = report.points[start..end].iter().map(|point| {
                    let id = *ids.entry(point.timestamp_ms).or_insert_with(accessibility::new_node_id);
                    (point.timestamp_ms, accessibility::group_with_id(
                        column![
                            text(format!("{} · +{} from retained start", timestamp(point.timestamp_ms), duration(point.timestamp_ms.saturating_sub(first))))
                                .size(style::SMALL).font(style::MEDIUM).width(Fill).wrapping(text::Wrapping::WordOrGlyph),
                            caption(format!("Frame rate {} frames per second · throughput {} MiB per second · missed frames {} percent", number(point.fps), number(point.bytes_per_second / 1_048_576.0), number(point.lost_percent)), p),
                            caption(format!("Bucket totals: {} acquired frames · {} missed frames · {} payload bytes", grouped(point.frames), grouped(point.lost), grouped(point.bytes)), p),
                        ].spacing(4).width(Fill).into(),
                        format!("History point {}", timestamp(point.timestamp_ms)),
                        id,
                    ))
                });
                details = details.push(iced::widget::keyed_column(rows).spacing(12).width(Fill));
            }
            content = content.push(accessibility::group_with_id(
                details.into(),
                "History snapshot".into(),
                self.snapshot_id,
            ));
        }
        content.into()
    }

    fn axes<'a>(&self, report: &AnalyticsSnapshot, p: &'static Palette) -> Element<'a, Message> {
        let first = report
            .points
            .first()
            .map_or(report.started_at_ms, |p| p.timestamp_ms);
        let last = report.points.last().map_or(first, |p| p.timestamp_ms);
        let span = last.saturating_sub(first).max(1000);
        let time_axis = if report.points.is_empty() {
            format!(
                "There is no retained measurement to define a time origin. The empty line charts use a one-second placeholder horizontal scale; session start is {}.",
                timestamp(report.started_at_ms)
            )
        } else {
            format!(
                "The three line charts share a horizontal scale from zero to {}. Zero is the first retained measurement at {}, not session start ({}).",
                duration(span),
                timestamp(first),
                timestamp(report.started_at_ms)
            )
        };
        let mut content = column![
            heading("Axes and aggregation", p),
            caption(time_axis, p),
            caption(format!("Vertical scales, rounded: frame rate zero to {} frames per second; throughput zero to {} MiB per second; missed frames zero to {} percent.", number(axis_max(report, |p| p.fps)), number(axis_max(report, |p| p.bytes_per_second / 1_048_576.0)), number(axis_max(report, |p| p.lost_percent))), p),
            caption("Each point combines retained samples in one time bucket. Its timestamp is the last sample in that bucket. Frame/byte rates are bucket averages; missed percentage uses usable rate deltas; counts are observed bucket totals. Counts may include an initial counter baseline, so missed percentage can differ from a ratio of the displayed counts. A bucket can cover several sampling intervals. Lines connect measurements; they do not supply values for missing samples.".into(), p),
        ].spacing(6).width(Fill);
        if report.points.len() <= 1 {
            content = content.push(caption("With fewer than two points, the horizontal axis uses a one-second display span. A vertical scale has at least 1.18 units of display range; the scale itself does not imply a measured value.".into(), p));
        }
        let mut cameras: Vec<_> = report.cameras.iter().collect();
        cameras.sort_by_key(|camera| std::cmp::Reverse(camera.bytes));
        cameras.truncate(5);
        let maximum = cameras
            .iter()
            .map(|camera| camera.bytes)
            .max()
            .unwrap_or(0)
            .max(1);
        content = content.push(caption(if cameras.is_empty() {
            "No retained camera data-volume measurements are available. There are no camera bars to compare.".into()
        } else {
            format!("Camera data-volume bars compare up to five of the largest session payload totals, from zero to {} bytes. These are cumulative totals, separate from the time-bucket rates.", grouped(maximum))
        }, p));
        let mut ids = self.camera_ids.borrow_mut();
        ids.retain(|id, _| cameras.iter().any(|camera| &camera.id == id));
        for camera in cameras {
            let id = *ids
                .entry(camera.id.clone())
                .or_insert_with(accessibility::new_node_id);
            content = content.push(accessibility::group_with_id(
                caption(
                    format!(
                        "{} ({}) · {} session payload bytes",
                        camera.name,
                        camera.id,
                        grouped(camera.bytes)
                    ),
                    p,
                ),
                format!("Camera data volume {}", camera.id),
                id,
            ));
        }
        accessibility::group_with_id(content.into(), "Axes and aggregation".into(), self.axes_id)
    }
}

fn caption<'a>(value: String, p: &'static Palette) -> Element<'a, Message> {
    text(value)
        .size(style::SMALL)
        .color(p.secondary)
        .width(Fill)
        .wrapping(text::Wrapping::WordOrGlyph)
        .into()
}

fn pages(count: usize) -> usize {
    count.div_ceil(PAGE_SIZE).max(1)
}

fn number(value: f64) -> String {
    if value != 0.0 && value.abs() < 0.005 {
        format!("{value:.2e}")
    } else if value.abs() >= 1_000_000.0 {
        format!("{value:.3e}")
    } else {
        format!("{value:.2}")
    }
}

pub(super) fn timestamp(value: u64) -> String {
    i64::try_from(value)
        .ok()
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map(|date| date.format("%Y-%m-%d %H:%M:%S%.3f UTC").to_string())
        .unwrap_or_else(|| format!("{value} milliseconds since Unix epoch"))
}

fn duration(ms: u64) -> String {
    let seconds = ms as f64 / 1000.0;
    if ms >= 3_600_000 {
        format!(
            "{} hours {} minutes {:.3} seconds",
            ms / 3_600_000,
            ms / 60_000 % 60,
            seconds % 60.0
        )
    } else if ms >= 60_000 {
        format!("{} minutes {:.3} seconds", ms / 60_000, seconds % 60.0)
    } else {
        format!("{seconds:.3} seconds")
    }
}

fn axis_max(report: &AnalyticsSnapshot, value: impl Fn(&AnalyticsPoint) -> f64) -> f64 {
    report
        .points
        .iter()
        .map(value)
        .fold(0.0_f64, f64::max)
        .max(1.0)
        * 1.18
}

fn overview(report: &AnalyticsSnapshot) -> Vec<String> {
    let Some(first) = report.points.first() else {
        return vec!["No retained history points are available. The line charts have no measured rates to describe.".into()];
    };
    let mut lines = vec![format!(
        "Three line charts describe retained measurements beginning {}. Elapsed chart time starts here, separately from session start.{}",
        timestamp(first.timestamp_ms),
        if report.points.len() == 1 {
            " Only one point is available; a change between measurements cannot be established."
        } else {
            ""
        },
    )];
    for (name, unit, value) in [
        (
            "Frame rate",
            "frames per second",
            (|p: &AnalyticsPoint| p.fps) as fn(&AnalyticsPoint) -> f64,
        ),
        (
            "Throughput",
            "MiB per second",
            (|p: &AnalyticsPoint| p.bytes_per_second / 1_048_576.0) as fn(&AnalyticsPoint) -> f64,
        ),
        (
            "Missed frames",
            "percent",
            (|p: &AnalyticsPoint| p.lost_percent) as fn(&AnalyticsPoint) -> f64,
        ),
    ] {
        let last = report.points.last().unwrap();
        let minimum = report
            .points
            .iter()
            .map(value)
            .fold(f64::INFINITY, f64::min);
        let peak = report
            .points
            .iter()
            .max_by(|a, b| value(a).total_cmp(&value(b)))
            .unwrap();
        lines.push(format!(
            "{name}: first {} → last {} {unit}; minimum {}; peak {} at +{}.",
            number(value(first)),
            number(value(last)),
            number(minimum),
            number(value(peak)),
            duration(peak.timestamp_ms.saturating_sub(first.timestamp_ms))
        ));
    }
    lines
}

#[cfg(test)]
mod tests;
