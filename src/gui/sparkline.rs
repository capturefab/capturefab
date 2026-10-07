//! Small line charts of a value over the last little while, so a dip or a
//! stall reads at a glance next to the number it explains.
use iced::widget::canvas::{self, Path, Stroke};
use iced::{Color, Point, Rectangle, Renderer, Size, Theme, mouse};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Frame rates: a sample every 250 ms over the last half minute.
const PERIOD: Duration = Duration::from_millis(250);
const SPAN: Duration = Duration::from_secs(30);
/// What a line needs before it shows a trend rather than a stub.
const MIN_SAMPLES: usize = 3;
const MIN_SPAN: Duration = Duration::from_millis(1750);

/// One sample: the value, and whether something was lost since the last one.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sample {
    pub value: f32,
    pub flagged: bool,
}

/// Samples of a value over a fixed span of time, taken at most once per
/// period however often they are offered.
#[derive(Clone, Debug)]
pub struct History {
    samples: VecDeque<(Instant, Sample)>,
    period: Duration,
    span: Duration,
    /// The cumulative counter at the last sample, to flag increases.
    counter: Option<u64>,
}

impl Default for History {
    fn default() -> Self {
        Self::every(PERIOD, SPAN)
    }
}

impl History {
    /// Samples at most once per `period`, kept for `span`.
    pub fn every(period: Duration, span: Duration) -> Self {
        Self {
            samples: VecDeque::new(),
            period,
            span,
            counter: None,
        }
    }

    /// Record `value` at `now` if a period has passed; the sample is flagged
    /// when the cumulative `counter` rose since the previous one.
    pub fn offer(&mut self, now: Instant, value: f64, counter: u64) {
        if self
            .samples
            .back()
            .is_some_and(|(taken, _)| now.saturating_duration_since(*taken) < self.period)
        {
            return;
        }
        let flagged = self.counter.is_some_and(|previous| counter > previous);
        self.counter = Some(counter);
        let value = if value.is_finite() {
            value.max(0.0)
        } else {
            0.0
        } as f32;
        self.samples.push_back((now, Sample { value, flagged }));
        while self
            .samples
            .front()
            .is_some_and(|(taken, _)| now.saturating_duration_since(*taken) > self.span)
        {
            self.samples.pop_front();
        }
    }

    pub fn clear(&mut self) {
        self.samples.clear();
        self.counter = None;
    }

    /// Enough samples, far enough apart, for a line that shows a trend.
    pub fn ready(&self) -> bool {
        self.samples.len() >= MIN_SAMPLES && self.elapsed() >= MIN_SPAN
    }

    pub fn samples(&self) -> impl ExactSizeIterator<Item = Sample> + '_ {
        self.samples.iter().map(|(_, sample)| *sample)
    }

    /// The newest value.
    pub fn last(&self) -> Option<f32> {
        self.samples.back().map(|(_, sample)| sample.value)
    }

    /// Lowest and highest value shown.
    pub fn range(&self) -> Option<(f32, f32)> {
        self.samples().fold(None, |range, s| {
            Some(range.map_or((s.value, s.value), |(lo, hi): (f32, f32)| {
                (lo.min(s.value), hi.max(s.value))
            }))
        })
    }

    /// How many samples were flagged.
    pub fn flagged(&self) -> usize {
        self.samples().filter(|s| s.flagged).count()
    }

    /// The time the samples span, in whole seconds.
    pub fn seconds(&self) -> u64 {
        self.elapsed().as_secs()
    }

    fn elapsed(&self) -> Duration {
        match (self.samples.front(), self.samples.back()) {
            (Some((first, _)), Some((last, _))) => last.saturating_duration_since(*first),
            _ => Duration::ZERO,
        }
    }
}

/// Draws a history as a line over a soft fill, scaled from zero so a drop
/// looks as large as it is, with flagged samples marked and the newest dotted.
pub struct Sparkline<'a> {
    pub history: &'a History,
    pub color: Color,
    pub flag: Color,
    /// The top of the scale; it grows to fit larger values.
    pub floor: f32,
}

impl<Message> canvas::Program<Message> for Sparkline<'_> {
    type State = ();

    fn draw(
        &self,
        _state: &(),
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<canvas::Geometry> {
        let mut frame = canvas::Frame::new(renderer, bounds.size());
        let points = points(self.history, bounds.size(), self.floor);
        let (Some(first), Some(last)) = (points.first(), points.last()) else {
            return vec![frame.into_geometry()];
        };
        let bottom = bounds.height;
        let area = Path::new(|b| {
            b.move_to(Point::new(first.x, bottom));
            for point in &points {
                b.line_to(*point);
            }
            b.line_to(Point::new(last.x, bottom));
            b.close();
        });
        frame.fill(
            &area,
            Color {
                a: self.color.a * 0.16,
                ..self.color
            },
        );
        let line = Path::new(|b| {
            b.move_to(*first);
            for point in &points[1..] {
                b.line_to(*point);
            }
        });
        frame.stroke(
            &line,
            Stroke::default()
                .with_color(self.color)
                .with_width(1.25)
                .with_line_join(canvas::LineJoin::Round),
        );
        for (sample, point) in self.history.samples().zip(&points) {
            if sample.flagged {
                frame.fill_rectangle(
                    Point::new(point.x - 0.75, 0.0),
                    Size::new(1.5, bottom),
                    Color {
                        a: self.flag.a * 0.55,
                        ..self.flag
                    },
                );
            }
        }
        frame.fill(&Path::circle(*last, 1.75), self.color);
        vec![frame.into_geometry()]
    }
}

/// Where each sample lands in `size`: placed by its age, so the newest is
/// always at the right edge and a gap in sampling shows as a long segment.
fn points(history: &History, size: Size, floor: f32) -> Vec<Point> {
    let Some((newest, _)) = history.samples.back().filter(|_| history.ready()) else {
        return Vec::new();
    };
    let inset = 2.0;
    let top = history
        .range()
        .map_or(floor, |(_, hi)| hi.max(floor))
        .max(f32::EPSILON);
    let width = size.width - inset * 2.0;
    let height = size.height - inset * 2.0;
    let span = history.span.as_secs_f32().max(f32::EPSILON);
    history
        .samples
        .iter()
        .map(|(taken, s)| {
            let age = newest.saturating_duration_since(*taken).as_secs_f32() / span;
            Point::new(
                inset + width * (1.0 - age.min(1.0)),
                inset + height * (1.0 - (s.value / top).min(1.0)),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_are_spaced_by_period_and_flag_losses() {
        let start = Instant::now();
        let mut history = History::default();
        history.offer(start, 30.0, 0);
        // Offered again too soon: ignored.
        history.offer(start + Duration::from_millis(60), 10.0, 5);
        history.offer(start + PERIOD, 29.0, 2);
        history.offer(start + PERIOD * 2, f64::NAN, 2);
        let samples: Vec<_> = history.samples().collect();
        assert_eq!(
            samples,
            [
                Sample {
                    value: 30.0,
                    flagged: false
                },
                Sample {
                    value: 29.0,
                    flagged: true
                },
                Sample {
                    value: 0.0,
                    flagged: false
                },
            ]
        );
        assert_eq!(history.range(), Some((0.0, 30.0)));
        assert_eq!(history.flagged(), 1);
    }

    #[test]
    fn history_keeps_only_its_span() {
        let start = Instant::now();
        let mut history = History::default();
        for i in 0..130 {
            history.offer(start + PERIOD * i, i as f64, 0);
        }
        assert_eq!(history.samples().len(), 121);
        assert_eq!(history.samples().next().unwrap().value, 9.0);
        assert_eq!(history.seconds(), 30);
    }

    #[test]
    fn points_are_placed_by_age_scaled_from_zero() {
        let start = Instant::now();
        let mut history = History::every(Duration::from_secs(1), Duration::from_secs(10));
        history.offer(start, 10.0, 0);
        history.offer(start + Duration::from_secs(1), 10.0, 0);
        // Not yet two seconds of history: no line.
        assert!(points(&history, Size::new(104.0, 24.0), 1.0).is_empty());
        // A slow reading leaves a long gap.
        history.offer(start + Duration::from_secs(6), 20.0, 0);
        let points = points(&history, Size::new(104.0, 24.0), 1.0);
        let expected = [(42.0, 12.0), (52.0, 12.0), (102.0, 2.0)];
        assert_eq!(points.len(), expected.len());
        for (point, (x, y)) in points.iter().zip(expected) {
            assert!(
                (point.x - x).abs() < 1e-3 && (point.y - y).abs() < 1e-3,
                "{point:?}"
            );
        }
    }
}
