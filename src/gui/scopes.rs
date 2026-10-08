//! Calibration scopes floating over the stage: a histogram of the picture's
//! levels with clipping, and focus scores for a region the user drags over
//! the image.
use super::*;
use crate::frame::{Exposure, Sharpness};
use iced::advanced::text::Alignment as TextAlign;
use iced::widget::canvas::{self, Frame, Geometry, LineCap, LineDash, Path, Stroke, gradient};
use iced::widget::{Row, column};
use iced::{Point, Rectangle, Renderer, Vector, alignment, mouse};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

/// Width of a scope card.
const WIDTH: f32 = 272.0;
/// Width of a scope card on a small stage; see `COMPACT_STAGE`.
const COMPACT_WIDTH: f32 = 236.0;
/// A stage narrower or shorter than this gets compact scope cards, so the
/// picture stays in view.
const COMPACT_STAGE: Size = Size::new(700.0, 560.0);
/// Clipping above this share of samples is called out.
const CLIP_WARN: f32 = 0.005;
/// Focus scores kept for the trace.
const HISTORY: usize = 120;
/// At this share of the peak a region comes into focus...
const IN_FOCUS: f32 = 0.97;
/// ...and below this one it leaves it again, so scores jittering around
/// the peak do not flicker between the two.
const IN_FOCUS_EXIT: f32 = 0.90;
/// Frames measured before the peak means anything.
const SETTLE: usize = 10;
/// Smallest region side, in sensor pixels.
const MIN_PIXELS: f32 = 16.0;
/// How close to an edge or corner of the region grabs it, in screen points.
const GRIP: f32 = 8.0;

const RED: Color = Color::from_rgb(1.0, 0.33, 0.31);
const GREEN: Color = Color::from_rgb(0.25, 0.85, 0.42);
const BLUE: Color = Color::from_rgb(0.33, 0.58, 1.0);

/// A focus measure the user picks; all are computed, one is charted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Metric {
    #[default]
    Laplacian,
    Tenengrad,
    Brenner,
    Variance,
}

impl Metric {
    pub const ALL: [Metric; 4] = [
        Metric::Laplacian,
        Metric::Tenengrad,
        Metric::Brenner,
        Metric::Variance,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Metric::Laplacian => "Laplacian",
            Metric::Tenengrad => "Tenengrad",
            Metric::Brenner => "Brenner",
            Metric::Variance => "Variance",
        }
    }

    fn hint(self) -> &'static str {
        match self {
            Metric::Laplacian => "Variance of the Laplacian: responds to fine edges",
            Metric::Tenengrad => "Squared Sobel gradient: steady on strong edges and in noise",
            Metric::Brenner => "Squared difference two pixels apart: favors fine texture",
            Metric::Variance => "Normalized intensity variance: contrast based, gentle on noise",
        }
    }

    fn of(self, s: &Sharpness) -> f32 {
        match self {
            Metric::Laplacian => s.laplacian,
            Metric::Tenengrad => s.tenengrad,
            Metric::Brenner => s.brenner,
            Metric::Variance => s.variance,
        }
    }
}

/// The focus region, its scores over recent frames and the best seen since
/// the region last changed.
pub struct FocusMeter {
    pub open: bool,
    /// Normalized to the frame: 0–1 on both axes.
    pub region: Rectangle,
    pub metric: Metric,
    history: VecDeque<Sharpness>,
    peak: Sharpness,
    /// In focus, with hysteresis; see `IN_FOCUS_EXIT`.
    sharp: bool,
    /// How many times the region came back into focus after leaving it.
    locks: u64,
}

impl FocusMeter {
    pub fn new(open: bool, region: [f32; 4], metric: Metric) -> Self {
        Self {
            open,
            region: sanitize(region),
            metric,
            history: VecDeque::with_capacity(HISTORY),
            peak: Sharpness::default(),
            sharp: false,
            locks: 0,
        }
    }

    pub fn saved_region(&self) -> [f32; 4] {
        let r = self.region;
        [r.x, r.y, r.width, r.height]
    }

    /// The region in pixels of a `width` × `height` frame.
    pub fn pixels(&self, width: u32, height: u32) -> [u32; 4] {
        let (w, h) = (width as f32, height as f32);
        let r = self.region;
        [
            (r.x * w).round() as u32,
            (r.y * h).round() as u32,
            (r.width * w).round().max(1.0) as u32,
            (r.height * h).round().max(1.0) as u32,
        ]
    }

    pub fn record(&mut self, score: Sharpness) {
        let settled = self.history.len() >= SETTLE;
        if self.history.len() == HISTORY {
            self.history.pop_front();
        }
        self.history.push_back(score);
        let peak = &mut self.peak;
        peak.laplacian = peak.laplacian.max(score.laplacian);
        peak.tenengrad = peak.tenengrad.max(score.tenengrad);
        peak.brenner = peak.brenner.max(score.brenner);
        peak.variance = peak.variance.max(score.variance);
        let was = self.sharp;
        let enough = if was { IN_FOCUS_EXIT } else { IN_FOCUS };
        self.sharp =
            self.history.len() >= SETTLE && self.share().is_some_and(|share| share >= enough);
        // Settling at a fresh peak is no lock-on; coming back to it is.
        if self.sharp && !was && settled {
            self.locks += 1;
        }
    }

    pub fn reset(&mut self) {
        self.history.clear();
        self.peak = Sharpness::default();
        self.sharp = false;
    }

    pub fn latest(&self) -> Option<f32> {
        self.history.back().map(|s| self.metric.of(s))
    }

    fn peak(&self) -> f32 {
        self.metric.of(&self.peak)
    }

    /// The latest score as a share of the peak, 0–1.
    pub fn share(&self) -> Option<f32> {
        let peak = self.peak();
        self.latest()
            .map(|latest| if peak > 0.0 { latest / peak } else { 0.0 })
    }

    /// Whether the latest score is near a peak established over several
    /// frames; a lone first frame is its own peak and says nothing.
    pub fn in_focus(&self) -> bool {
        self.sharp
    }

    /// How many times the region came back into focus, for the lock-on.
    pub fn locks(&self) -> u64 {
        self.locks
    }

    fn trace(&self) -> Vec<f32> {
        self.history.iter().map(|s| self.metric.of(s)).collect()
    }
}

/// A saved region made valid: finite, inside the frame and not empty.
fn sanitize([x, y, w, h]: [f32; 4]) -> Rectangle {
    if ![x, y, w, h].iter().all(|v| v.is_finite()) {
        return sanitize(DEFAULT_REGION);
    }
    // Any positive size is kept: the editor's pixel minimum depends on the frame.
    let (w, h) = (w.clamp(1e-4, 1.0), h.clamp(1e-4, 1.0));
    Rectangle::new(
        Point::new(x.clamp(0.0, 1.0 - w), y.clamp(0.0, 1.0 - h)),
        Size::new(w, h),
    )
}

/// The middle quarter of the frame.
pub const DEFAULT_REGION: [f32; 4] = [0.375, 0.375, 0.25, 0.25];

/// A score in four significant figures at most, e.g. "0.42", "41.7", "1203", "12.4k".
fn score(value: f32) -> String {
    match value {
        v if v >= 10_000.0 => format!("{:.1}k", v / 1000.0),
        v if v >= 100.0 => format!("{v:.0}"),
        v if v >= 10.0 => format!("{v:.1}"),
        v => format!("{v:.2}"),
    }
}

fn percent(share: f32) -> String {
    let p = share * 100.0;
    if p <= 0.0 {
        "0%".into()
    } else if p < 0.1 {
        "<0.1%".into()
    } else if p < 10.0 {
        format!("{p:.1}%")
    } else {
        format!("{p:.0}%")
    }
}

impl Workbench {
    /// The scope cards stacked in the stage's top right corner, compact on
    /// a small stage.
    pub(super) fn scopes(&self) -> Option<Element<'_, Message>> {
        let exposure = self.exposure_slide.get(self.now);
        let focus = self.focus_slide.get(self.now);
        if exposure <= 0.01 && focus <= 0.01 {
            return None;
        }
        Some(
            responsive(move |stage| {
                let compact =
                    stage.width < COMPACT_STAGE.width || stage.height < COMPACT_STAGE.height;
                let mut cards = column![]
                    .spacing(if compact { 6 } else { 8 })
                    .width(if compact { COMPACT_WIDTH } else { WIDTH });
                if exposure > 0.01
                    && let Some(card) = self.exposure_card(exposure, compact)
                {
                    cards = cards.push(card);
                }
                if focus > 0.01 {
                    cards = cards.push(self.focus_card(focus, compact));
                }
                // Opaque, so clicks on a card never reach the focus region under it.
                container(opaque(cards))
                    .align_right(Fill)
                    .align_top(Fill)
                    .padding(if compact { 8 } else { 12 })
                    .into()
            })
            .into(),
        )
    }

    fn exposure_card(&self, shown: f32, compact: bool) -> Option<Element<'_, Message>> {
        let exposure = self.exposure.as_ref()?;
        let ink = |color: Color| fade(color, shown);
        let stage = style::STAGE;
        let clip = |label: &'static str, share: f32| {
            let over = share > CLIP_WARN;
            let color = ink(if over { stage.warn } else { stage.secondary });
            row![
                dot(color, 5.0),
                text(format!("{label} {}", percent(share)))
                    .size(style::CAPTION)
                    .font(if over { style::MEDIUM } else { style::SANS })
                    .color(color),
            ]
            .spacing(5)
            .align_y(Alignment::Center)
        };
        // Not "Exposure": that is the control in the inspector.
        let header = row![
            text("Histogram")
                .size(style::SMALL)
                .font(style::SEMIBOLD)
                .color(ink(stage.text)),
            space::horizontal(),
            text(format!("Mean {:.0}%", exposure.mean * 100.0))
                .size(style::CAPTION)
                .color(ink(stage.secondary)),
        ]
        .align_y(Alignment::Center);
        let footer = row![
            tip(
                clip("Shadows", exposure.shadows),
                "Samples crushed to black"
            ),
            space::horizontal(),
            tip(
                clip("Highlights", exposure.highlights),
                "Samples with a channel blown out"
            ),
        ]
        .align_y(Alignment::Center);
        let scope = canvas::Canvas::new(ExposureScope {
            exposure: exposure.clone(),
            shown,
            warn: stage.warn,
        })
        .width(Fill)
        .height(if compact { 44 } else { 72 });
        Some(
            container(column![header, scope, footer].spacing(if compact { 6 } else { 8 }))
                .padding(if compact { 10 } else { 12 })
                .width(Fill)
                .style(style::overlay(shown))
                .into(),
        )
    }

    fn focus_card(&self, shown: f32, compact: bool) -> Element<'_, Message> {
        let meter = &self.focus;
        let ink = |color: Color| fade(color, shown);
        let stage = style::STAGE;
        let share = meter.share();
        let sharp = meter.in_focus();
        let metrics = Row::with_children(Metric::ALL.map(|metric| {
            let selected = metric == meter.metric;
            tip(
                button(
                    text(metric.label())
                        .size(style::CAPTION)
                        .font(if selected {
                            style::SEMIBOLD
                        } else {
                            style::SANS
                        })
                        .width(Fill)
                        .align_x(Alignment::Center)
                        .wrapping(text::Wrapping::None),
                )
                .width(Fill)
                .padding([3, if compact { 0 } else { 2 }])
                .style(style::glass_segment(selected, shown))
                .on_press(Message::FocusMetric(metric)),
                metric.hint(),
            )
        }))
        .spacing(2);
        let header = row![
            text("Focus")
                .size(style::SMALL)
                .font(style::SEMIBOLD)
                .color(ink(stage.text)),
            space::horizontal(),
            tip(
                button(icon(Icon::Reset, 12.0, ink(stage.secondary)))
                    .padding(3)
                    .style(style::on_glass(false, shown))
                    .on_press(Message::ResetFocusPeak),
                "Reset peak",
            ),
            tip(
                button(icon(Icon::Close, 12.0, ink(stage.secondary)))
                    .padding(3)
                    .style(style::on_glass(false, shown))
                    .on_press(Message::ToggleFocusRegion),
                "Hide focus region",
            ),
        ]
        .spacing(2)
        .align_y(Alignment::Center);
        let lit = ink(if sharp { stage.live } else { stage.text });
        let verdict: Element<'_, Message> = if sharp {
            row![
                icon(Icon::Check, 11.0, ink(stage.live)),
                text("In focus")
                    .size(style::CAPTION)
                    .color(ink(stage.secondary)),
            ]
            .spacing(4)
            .align_y(Alignment::Center)
            .into()
        } else {
            text(if meter.history.len() >= SETTLE {
                "Adjust to raise the score"
            } else if share.is_some() {
                "Measuring…"
            } else {
                "Waiting for a frame"
            })
            .size(style::CAPTION)
            .color(ink(stage.secondary))
            .into()
        };
        let reading = row![
            text(meter.latest().map_or("–".into(), score))
                .size(if compact {
                    style::TITLE
                } else {
                    style::DISPLAY
                })
                .font(style::SEMIBOLD)
                .color(lit),
            space::horizontal(),
            column![
                text(share.map_or(String::new(), |share| format!(
                    "{:.0}% of peak",
                    share * 100.0
                )))
                .size(style::SMALL)
                .font(style::MEDIUM)
                .color(lit),
                verdict,
            ]
            .align_x(Alignment::End)
            .spacing(1),
        ]
        .align_y(Alignment::Center);
        let trace = canvas::Canvas::new(Trace {
            values: meter.trace(),
            peak: meter.peak(),
            color: if sharp { stage.live } else { stage.accent },
            shown,
        })
        .width(Fill)
        .height(if compact { 26 } else { 40 });
        container(column![header, metrics, reading, trace].spacing(if compact { 6 } else { 8 }))
            .padding(if compact { 10 } else { 12 })
            .width(Fill)
            .style(style::overlay(shown))
            .into()
    }

    /// The draggable focus region over the image, when shown. It reads the
    /// picture's own scale, so it stays on the picture while a zoom eases.
    pub(super) fn focus_overlay(&self, shown: &Shown) -> Option<Element<'_, Message>> {
        let t = self.focus_slide.get(self.now);
        if t <= 0.01 {
            return None;
        }
        let label = match (self.focus.latest(), self.focus.share()) {
            (Some(latest), Some(share)) => format!("{}  {:.0}%", score(latest), share * 100.0),
            _ => self.focus.metric.label().into(),
        };
        Some(
            canvas::Canvas::new(RegionEditor {
                region: self.focus.region,
                native: shown.size(),
                scale: self.view_scale(),
                label,
                color: if self.focus.in_focus() {
                    style::STAGE.live
                } else {
                    style::STAGE.accent
                },
                shown: t,
                lock: self.stage_ui.lock.get(self.now),
            })
            .width(Fill)
            .height(Fill)
            .into(),
        )
    }
}

/// Luminance as a soft filled curve with red, green and blue curves behind it
/// for color frames, on a square-root scale so small populations stay
/// visible; clipped ends glow in `warn`.
struct ExposureScope {
    exposure: Exposure,
    shown: f32,
    warn: Color,
}

impl ExposureScope {
    /// The area under `bins`, scaled so `max` reaches the top.
    fn area(bins: &[u32; 64], max: f32, size: Size) -> Path {
        let step = size.width / bins.len() as f32;
        let y = |count: u32| size.height - (count as f32 / max).sqrt() * size.height;
        Path::new(|path| {
            path.move_to(Point::new(0.0, size.height));
            path.line_to(Point::new(0.0, y(bins[0])));
            for (i, &count) in bins.iter().enumerate() {
                path.line_to(Point::new((i as f32 + 0.5) * step, y(count)));
            }
            path.line_to(Point::new(size.width, y(bins[bins.len() - 1])));
            path.line_to(Point::new(size.width, size.height));
            path.close();
        })
    }
}

impl canvas::Program<Message> for ExposureScope {
    type State = ();

    fn draw(
        &self,
        _state: &(),
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let mut frame = Frame::new(renderer, bounds.size());
        let size = bounds.size();
        let a = |color: Color, alpha: f32| Color {
            a: alpha * self.shown,
            ..color
        };
        // Quarter tones, and the baseline.
        for quarter in 1..4 {
            let x = (size.width * quarter as f32 / 4.0).round() + 0.5;
            frame.stroke(
                &Path::line(Point::new(x, 0.0), Point::new(x, size.height)),
                Stroke::default()
                    .with_color(a(Color::WHITE, 0.07))
                    .with_width(1.0),
            );
        }
        frame.fill_rectangle(
            Point::new(0.0, size.height - 1.0),
            Size::new(size.width, 1.0),
            a(Color::WHITE, 0.14),
        );
        let e = &self.exposure;
        let max = std::iter::once(&e.luma)
            .chain(e.channels.iter().flatten())
            .flat_map(|bins| bins.iter().copied())
            .max()
            .unwrap_or(0)
            .max(1) as f32;
        let curve = |frame: &mut Frame, bins: &[u32; 64], color: Color, fill: f32, line: f32| {
            let path = Self::area(bins, max, size);
            frame.fill(
                &path,
                gradient::Linear::new(Point::ORIGIN, Point::new(0.0, size.height))
                    .add_stop(0.0, a(color, fill))
                    .add_stop(1.0, a(color, fill * 0.25)),
            );
            frame.stroke(
                &path,
                Stroke::default()
                    .with_color(a(color, line))
                    .with_width(1.2)
                    .with_line_join(canvas::LineJoin::Round),
            );
        };
        if let Some(channels) = &e.channels {
            for (bins, color) in channels.iter().zip([RED, GREEN, BLUE]) {
                curve(&mut frame, bins, color, 0.22, 0.75);
            }
            curve(&mut frame, &e.luma, Color::WHITE, 0.16, 0.9);
        } else {
            curve(&mut frame, &e.luma, Color::WHITE, 0.4, 0.95);
        }
        // Clipped ends glow from the edge inward.
        let glow = 16.0;
        for (share, from, to) in [
            (e.shadows, 0.0, glow),
            (e.highlights, size.width, size.width - glow),
        ] {
            if share > CLIP_WARN {
                let x = from.min(to);
                frame.fill_rectangle(
                    Point::new(x, 0.0),
                    Size::new(glow, size.height),
                    gradient::Linear::new(Point::new(from, 0.0), Point::new(to, 0.0))
                        .add_stop(0.0, a(self.warn, 0.55))
                        .add_stop(1.0, a(self.warn, 0.0)),
                );
                frame.fill_rectangle(
                    Point::new(if from > 0.0 { from - 2.0 } else { 0.0 }, 0.0),
                    Size::new(2.0, size.height),
                    a(self.warn, 0.95),
                );
            }
        }
        vec![frame.into_geometry()]
    }
}

/// Recent focus scores as a line rising toward the dashed peak, newest on the
/// right.
struct Trace {
    values: Vec<f32>,
    peak: f32,
    color: Color,
    shown: f32,
}

impl canvas::Program<Message> for Trace {
    type State = ();

    fn draw(
        &self,
        _state: &(),
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let mut frame = Frame::new(renderer, bounds.size());
        let size = bounds.size();
        let a = |color: Color, alpha: f32| Color {
            a: alpha * self.shown,
            ..color
        };
        let top = 4.0;
        frame.stroke(
            &Path::line(Point::new(0.0, top), Point::new(size.width, top)),
            Stroke {
                line_dash: LineDash {
                    segments: &[3.0, 3.0],
                    offset: 0,
                },
                ..Stroke::default()
                    .with_color(a(Color::WHITE, 0.28))
                    .with_width(1.0)
            },
        );
        if self.values.is_empty() || self.peak <= 0.0 {
            return vec![frame.into_geometry()];
        }
        let step = size.width / (HISTORY - 1) as f32;
        let start = size.width - (self.values.len() - 1) as f32 * step;
        let point = |i: usize, v: f32| {
            Point::new(
                start + i as f32 * step,
                size.height - (v / self.peak).clamp(0.0, 1.0) * (size.height - top),
            )
        };
        let line = Path::new(|path| {
            for (i, &v) in self.values.iter().enumerate() {
                if i == 0 {
                    path.move_to(point(i, v));
                } else {
                    path.line_to(point(i, v));
                }
            }
        });
        let area = Path::new(|path| {
            path.move_to(Point::new(start, size.height));
            for (i, &v) in self.values.iter().enumerate() {
                path.line_to(point(i, v));
            }
            path.line_to(Point::new(size.width, size.height));
            path.close();
        });
        frame.fill(
            &area,
            gradient::Linear::new(Point::new(0.0, top), Point::new(0.0, size.height))
                .add_stop(0.0, a(self.color, 0.35))
                .add_stop(1.0, a(self.color, 0.0)),
        );
        frame.stroke(
            &line,
            Stroke::default()
                .with_color(a(self.color, 1.0))
                .with_width(1.5)
                .with_line_join(canvas::LineJoin::Round),
        );
        let last = self.values.len() - 1;
        let end = point(last, self.values[last]);
        frame.fill(&Path::circle(end, 4.5), a(self.color, 0.3));
        frame.fill(&Path::circle(end, 2.5), a(self.color, 1.0));
        vec![frame.into_geometry()]
    }
}

/// What a press on the region editor holds on to.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Grip {
    /// The whole region.
    Move,
    /// The sides being dragged: left, top, right, bottom.
    Sides([bool; 4]),
    /// A new region from an anchor, drawn outside the old one.
    New,
}

#[derive(Default)]
pub struct Drag {
    held: Option<(Grip, Point, Rectangle)>,
    moved: bool,
}

/// The focus region over the image: everything outside it dims, its corners
/// read like a viewfinder, and its score rides on its top edge. Drag inside to
/// move it, an edge or corner to resize it, or outside to draw a new one.
struct RegionEditor {
    region: Rectangle,
    native: Size,
    scale: Option<f32>,
    label: String,
    color: Color,
    shown: f32,
    /// The lock-on as it plays, from 1 to 0: the corners swell and a halo
    /// glows, both peaking early on; under Reduce Motion only the halo.
    lock: f32,
}

impl RegionEditor {
    fn image(&self, bounds: Rectangle) -> Rectangle {
        preview::placement(
            Rectangle::new(Point::ORIGIN, bounds.size()),
            self.native,
            self.scale,
        )
    }

    fn on_screen(&self, image: Rectangle, region: Rectangle) -> Rectangle {
        Rectangle::new(
            Point::new(
                image.x + region.x * image.width,
                image.y + region.y * image.height,
            ),
            Size::new(region.width * image.width, region.height * image.height),
        )
    }

    /// `point` in normalized frame coordinates, kept inside the frame.
    fn normalized(image: Rectangle, point: Point) -> Point {
        Point::new(
            ((point.x - image.x) / image.width).clamp(0.0, 1.0),
            ((point.y - image.y) / image.height).clamp(0.0, 1.0),
        )
    }

    fn grip(&self, image: Rectangle, point: Point) -> Option<Grip> {
        let r = self.on_screen(image, self.region);
        let (right, bottom) = (r.x + r.width, r.y + r.height);
        let across = point.y > r.y - GRIP && point.y < bottom + GRIP;
        let along = point.x > r.x - GRIP && point.x < right + GRIP;
        let near = |a: f32, b: f32| (a - b).abs() < GRIP;
        // On a small region both edges are near: take the closer one.
        let left = across && near(point.x, r.x) && (point.x - r.x).abs() <= (point.x - right).abs();
        let right = across && near(point.x, right) && !left;
        let top = along && near(point.y, r.y) && (point.y - r.y).abs() <= (point.y - bottom).abs();
        let bottom = along && near(point.y, bottom) && !top;
        if left || top || right || bottom {
            Some(Grip::Sides([left, top, right, bottom]))
        } else if r.contains(point) {
            Some(Grip::Move)
        } else {
            None
        }
    }

    /// The smallest side of a region, normalized on each axis.
    fn min_size(&self) -> Size {
        Size::new(
            (MIN_PIXELS / self.native.width.max(1.0)).min(1.0),
            (MIN_PIXELS / self.native.height.max(1.0)).min(1.0),
        )
    }

    /// The region after dragging `grip` from `from` to `to`, all normalized.
    fn dragged(&self, grip: Grip, from: Point, to: Point, start: Rectangle) -> Rectangle {
        // A region kept from a larger frame may already be under the minimum.
        let min = self.min_size();
        let min = Size::new(min.width.min(start.width), min.height.min(start.height));
        let (dx, dy) = (to.x - from.x, to.y - from.y);
        match grip {
            Grip::Move => Rectangle::new(
                Point::new(
                    (start.x + dx).clamp(0.0, 1.0 - start.width),
                    (start.y + dy).clamp(0.0, 1.0 - start.height),
                ),
                start.size(),
            ),
            Grip::Sides([left, top, right, bottom]) => {
                let (mut x0, mut y0) = (start.x, start.y);
                let (mut x1, mut y1) = (start.x + start.width, start.y + start.height);
                if left {
                    x0 = (x0 + dx).clamp(0.0, x1 - min.width);
                }
                if right {
                    x1 = (x1 + dx).clamp(x0 + min.width, 1.0);
                }
                if top {
                    y0 = (y0 + dy).clamp(0.0, y1 - min.height);
                }
                if bottom {
                    y1 = (y1 + dy).clamp(y0 + min.height, 1.0);
                }
                Rectangle::new(Point::new(x0, y0), Size::new(x1 - x0, y1 - y0))
            }
            Grip::New => {
                let w = (to.x - from.x).abs().max(min.width);
                let h = (to.y - from.y).abs().max(min.height);
                let x = if to.x < from.x { from.x - w } else { from.x };
                let y = if to.y < from.y { from.y - h } else { from.y };
                Rectangle::new(
                    Point::new(x.clamp(0.0, 1.0 - w), y.clamp(0.0, 1.0 - h)),
                    Size::new(w, h),
                )
            }
        }
    }

    /// The current region centered on `point`, for a click outside it.
    fn centered(&self, point: Point) -> Rectangle {
        let r = self.region;
        Rectangle::new(
            Point::new(
                (point.x - r.width / 2.0).clamp(0.0, 1.0 - r.width),
                (point.y - r.height / 2.0).clamp(0.0, 1.0 - r.height),
            ),
            r.size(),
        )
    }
}

impl canvas::Program<Message> for RegionEditor {
    type State = Drag;

    fn update(
        &self,
        drag: &mut Drag,
        event: &canvas::Event,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> Option<canvas::Action<Message>> {
        let image = self.image(bounds);
        match event {
            canvas::Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                let point = cursor.position_in(bounds)?;
                let grip = match self.grip(image, point) {
                    Some(grip) => grip,
                    None if image.contains(point) => Grip::New,
                    None => return None,
                };
                drag.held = Some((grip, Self::normalized(image, point), self.region));
                drag.moved = false;
                Some(canvas::Action::capture())
            }
            canvas::Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                let (grip, from, start) = drag.held?;
                let to = Self::normalized(
                    image,
                    cursor.position_in(bounds).or_else(|| {
                        // Keep dragging past the stage's edge.
                        cursor
                            .position()
                            .map(|p| Point::new(p.x - bounds.x, p.y - bounds.y))
                    })?,
                );
                let distance =
                    (to.x - from.x).abs() * image.width + (to.y - from.y).abs() * image.height;
                drag.moved |= distance > 3.0;
                if !drag.moved {
                    return Some(canvas::Action::capture());
                }
                Some(
                    canvas::Action::publish(Message::FocusRegion(
                        self.dragged(grip, from, to, start),
                    ))
                    .and_capture(),
                )
            }
            canvas::Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                let (grip, from, _) = drag.held.take()?;
                if grip == Grip::New && !drag.moved {
                    return Some(
                        canvas::Action::publish(Message::FocusRegion(self.centered(from)))
                            .and_capture(),
                    );
                }
                Some(canvas::Action::capture())
            }
            _ => None,
        }
    }

    fn draw(
        &self,
        drag: &Drag,
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let mut frame = Frame::new(renderer, bounds.size());
        let image = self.image(bounds);
        let r = self.on_screen(image, self.region);
        let a = |color: Color, alpha: f32| Color {
            a: alpha * self.shown,
            ..color
        };
        // Dim the image around the region.
        let shade = a(Color::BLACK, 0.4);
        let (right, bottom) = (r.x + r.width, r.y + r.height);
        let (image_right, image_bottom) = (image.x + image.width, image.y + image.height);
        for (x, y, w, h) in [
            (image.x, image.y, image.width, r.y - image.y),
            (image.x, bottom, image.width, image_bottom - bottom),
            (image.x, r.y, r.x - image.x, r.height),
            (right, r.y, image_right - right, r.height),
        ] {
            if w > 0.0 && h > 0.0 {
                frame.fill_rectangle(Point::new(x, y), Size::new(w, h), shade);
            }
        }
        frame.stroke(
            &Path::rectangle(r.position(), r.size()),
            Stroke::default()
                .with_color(a(Color::WHITE, 0.6))
                .with_width(1.0),
        );
        // Locking on: 0 at either end, 1 at the height of it.
        let bump = (self.lock.clamp(0.0, 1.0) * std::f32::consts::PI).sin();
        if bump > 0.01 {
            frame.stroke(
                &Path::rectangle(
                    r.position() - Vector::new(3.0, 3.0),
                    Size::new(r.width + 6.0, r.height + 6.0),
                ),
                Stroke::default()
                    .with_color(a(self.color, 0.5 * bump))
                    .with_width(1.0),
            );
        }
        let swell = if motion::reduce_motion() { 0.0 } else { bump };
        // Viewfinder corners.
        let arm = (14.0 + 6.0 * swell).min(r.width / 3.0).min(r.height / 3.0);
        let corners = Path::new(|path| {
            for (x, y, sx, sy) in [
                (r.x, r.y, 1.0, 1.0),
                (right, r.y, -1.0, 1.0),
                (r.x, bottom, 1.0, -1.0),
                (right, bottom, -1.0, -1.0),
            ] {
                path.move_to(Point::new(x + sx * arm, y));
                path.line_to(Point::new(x, y));
                path.line_to(Point::new(x, y + sy * arm));
            }
        });
        frame.stroke(
            &corners,
            Stroke::default()
                .with_color(a(self.color, 1.0))
                .with_width(3.0 + swell)
                .with_line_cap(LineCap::Round),
        );
        // The score on a tag above the region, or its size while dragging.
        let label = if drag.moved && drag.held.is_some() {
            format!(
                "{} × {}",
                (self.region.width * self.native.width).round(),
                (self.region.height * self.native.height).round()
            )
        } else {
            self.label.clone()
        };
        let size = 11.0;
        let tag = Size::new(label.chars().count() as f32 * size * 0.62 + 14.0, 20.0);
        let above = r.y - tag.height - 6.0;
        let at = Point::new(
            r.x.clamp(0.0, (bounds.width - tag.width).max(0.0)),
            if above >= 0.0 { above } else { r.y + 6.0 },
        );
        frame.fill(
            &Path::rounded_rectangle(at, tag, 6.0.into()),
            a(Color::from_rgb8(0x1C, 0x1C, 0x1E), 0.82),
        );
        frame.fill_text(canvas::Text {
            content: label,
            position: Point::new(at.x + tag.width / 2.0, at.y + tag.height / 2.0),
            color: a(style::STAGE.text, 1.0),
            size: size.into(),
            font: style::MONO,
            align_x: TextAlign::Center,
            align_y: alignment::Vertical::Center,
            ..canvas::Text::default()
        });
        vec![frame.into_geometry()]
    }

    fn mouse_interaction(
        &self,
        drag: &Drag,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> mouse::Interaction {
        let image = self.image(bounds);
        let grip = match drag.held {
            Some((grip, ..)) => Some(grip),
            None => match cursor.position_in(bounds) {
                Some(point) => self.grip(image, point),
                None => return mouse::Interaction::None,
            },
        };
        match grip {
            Some(Grip::Move) if drag.held.is_some() => mouse::Interaction::Grabbing,
            Some(Grip::Move) => mouse::Interaction::Grab,
            Some(Grip::Sides([l, t, r, b])) => match (l || r, t || b) {
                (true, true) if (l && t) || (r && b) => mouse::Interaction::ResizingDiagonallyDown,
                (true, true) => mouse::Interaction::ResizingDiagonallyUp,
                (true, false) => mouse::Interaction::ResizingHorizontally,
                _ => mouse::Interaction::ResizingVertically,
            },
            Some(Grip::New) => mouse::Interaction::Crosshair,
            None if cursor
                .position_in(bounds)
                .is_some_and(|point| image.contains(point)) =>
            {
                mouse::Interaction::Crosshair
            }
            None => mouse::Interaction::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn editor() -> RegionEditor {
        RegionEditor {
            region: Rectangle::new(Point::new(0.25, 0.25), Size::new(0.5, 0.5)),
            native: Size::new(400.0, 200.0),
            scale: Some(1.0),
            label: String::new(),
            color: Color::WHITE,
            shown: 1.0,
            lock: 0.0,
        }
    }

    #[test]
    fn grips_follow_the_region_on_screen() {
        let editor = editor();
        // At 1:1 in a 600 × 400 stage the image spans (100, 100)–(500, 300),
        // so the region spans (200, 150)–(400, 250).
        let image = editor.image(Rectangle::new(Point::ORIGIN, Size::new(600.0, 400.0)));
        assert_eq!(
            image,
            Rectangle::new(Point::new(100.0, 100.0), Size::new(400.0, 200.0))
        );
        let grip = |x, y| editor.grip(image, Point::new(x, y));
        assert_eq!(grip(300.0, 200.0), Some(Grip::Move));
        assert_eq!(
            grip(201.0, 200.0),
            Some(Grip::Sides([true, false, false, false]))
        );
        assert_eq!(
            grip(399.0, 251.0),
            Some(Grip::Sides([false, false, true, true]))
        );
        assert_eq!(grip(150.0, 200.0), None);
    }

    #[test]
    fn drags_stay_inside_the_frame_and_above_the_minimum() {
        let editor = editor();
        let start = editor.region;
        let at = |x, y| Point::new(x, y);
        let moved = editor.dragged(Grip::Move, at(0.5, 0.5), at(2.0, -2.0), start);
        assert_eq!((moved.x, moved.y, moved.width), (0.5, 0.0, 0.5));
        let shrunk = editor.dragged(
            Grip::Sides([true, false, false, false]),
            at(0.25, 0.5),
            at(0.9, 0.5),
            start,
        );
        assert!((shrunk.width - MIN_PIXELS / 400.0).abs() < 1e-6);
        assert!((shrunk.x + shrunk.width - 0.75).abs() < 1e-6);
        let drawn = editor.dragged(Grip::New, at(0.8, 0.9), at(0.6, 0.5), start);
        assert!((drawn.x - 0.6).abs() < 1e-6 && (drawn.y - 0.5).abs() < 1e-6);
        assert!((drawn.width - 0.2).abs() < 1e-6 && (drawn.height - 0.4).abs() < 1e-6);
        assert_eq!(editor.centered(at(1.0, 0.0)).position(), at(0.5, 0.0));
        // A sliver at the right edge, under the minimum after a resolution drop.
        let sliver = Rectangle::new(at(0.99, 0.2), Size::new(0.01, 0.5));
        for sides in [[false, false, true, false], [true, false, false, false]] {
            let resized = editor.dragged(Grip::Sides(sides), at(0.99, 0.5), at(1.0, 0.5), sliver);
            assert!(resized.x >= 0.0 && resized.x + resized.width <= 1.0 + 1e-6);
        }
    }

    #[test]
    fn meter_tracks_the_peak_and_saved_regions_are_sanitized() {
        let mut meter = FocusMeter::new(true, [f32::NAN, 0.0, 1.0, 1.0], Metric::Tenengrad);
        assert_eq!(meter.saved_region(), DEFAULT_REGION);
        assert_eq!(meter.pixels(800, 600), [300, 225, 200, 150]);
        for tenengrad in [10.0, 40.0, 20.0] {
            meter.record(Sharpness {
                tenengrad,
                ..Sharpness::default()
            });
        }
        assert_eq!((meter.latest(), meter.share()), (Some(20.0), Some(0.5)));
        assert!(!meter.in_focus());
        for _ in 0..SETTLE {
            meter.record(Sharpness {
                tenengrad: 39.0,
                ..Sharpness::default()
            });
        }
        assert!(meter.in_focus());
        assert_eq!(meter.locks(), 0, "settling at the peak is no lock-on");
        // Hysteresis: in focus down to IN_FOCUS_EXIT, out until IN_FOCUS.
        let record = |meter: &mut FocusMeter, tenengrad| {
            meter.record(Sharpness {
                tenengrad,
                ..Sharpness::default()
            });
            meter.in_focus()
        };
        assert!(record(&mut meter, 38.0), "0.95 stays in focus");
        assert!(!record(&mut meter, 35.0), "0.875 leaves it");
        assert!(!record(&mut meter, 38.0), "0.95 is not back yet");
        assert_eq!(meter.locks(), 0);
        assert!(record(&mut meter, 39.5), "0.9875 is");
        assert_eq!(meter.locks(), 1, "and locks on");
        assert!(record(&mut meter, 39.6));
        assert_eq!(meter.locks(), 1, "once");
        meter.reset();
        assert!(!meter.in_focus());
        assert_eq!(meter.share(), None);
        let tiny = FocusMeter::new(false, [0.5, 0.5, 0.004, 0.007], Metric::default());
        assert_eq!(tiny.saved_region(), [0.5, 0.5, 0.004, 0.007]);
        let outside = FocusMeter::new(false, [0.9, 0.9, 0.5, 2.0], Metric::default());
        assert_eq!(outside.saved_region(), [0.5, 0.0, 0.5, 1.0]);
        assert_eq!(score(0.4234), "0.42");
        assert_eq!(score(1203.4), "1203");
        assert_eq!(score(12_345.0), "12.3k");
        assert_eq!(percent(0.0004), "<0.1%");
    }
}
