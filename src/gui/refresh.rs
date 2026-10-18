//! Pacing camera frames to the display.
//!
//! Camera frames are announced no faster than the window's screen refreshes,
//! so a fast camera does not rebuild the view more often than it can be seen,
//! and a 120 Hz display is not held to 60. iced reports no refresh rate, so on
//! macOS the screen is asked directly; there, redraws are not paced by vsync
//! and run ahead of a 60 Hz display. Elsewhere presentation blocks on vsync,
//! and the spacing of `window::frames` ticks is the rate the display delivers,
//! except under the software renderer, which presents without waiting.
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Redraw interval assumed until one is measured.
const DEFAULT: Duration = Duration::from_micros(16_667);
/// The fastest display paced to (240 Hz).
const FASTEST: Duration = Duration::from_micros(4_167);
/// Longer intervals are idle time between redraws, not a refresh rate.
const SLOWEST: Duration = Duration::from_millis(34);
const SAMPLES: usize = 32;
/// Intervals needed before the estimate is trusted.
const ENOUGH: usize = 12;
/// Redraws after which measuring stops even without an estimate, e.g. on a
/// display slower than `SLOWEST`, so an idle window does not keep redrawing.
const GIVE_UP: u32 = 90;
/// How long the window stays put after moving before it is measured again:
/// a drag moves it many times.
const SETTLE: Duration = Duration::from_millis(500);

pub(super) struct Refresh {
    last: Option<Instant>,
    intervals: VecDeque<Duration>,
    /// Redraws seen since the last recalibration.
    redraws: u32,
    measured: Duration,
    /// The interval the platform reports for the window's screen.
    reported: Option<Duration>,
    /// Redraws time the drawing, not the display: never measure.
    unpaced: bool,
    /// When the window last moved, until it is measured again.
    moved: Option<Instant>,
}

impl Default for Refresh {
    fn default() -> Self {
        Self {
            last: None,
            intervals: VecDeque::with_capacity(SAMPLES),
            redraws: 0,
            measured: DEFAULT,
            reported: None,
            unpaced: false,
            moved: None,
        }
    }
}

impl Refresh {
    /// Note a vsync-paced redraw at `at`.
    pub(super) fn redraw(&mut self, at: Instant) {
        if self.unpaced {
            return;
        }
        self.redraws = self.redraws.saturating_add(1);
        let Some(last) = self.last.replace(at) else {
            return;
        };
        let interval = at.saturating_duration_since(last);
        if !(FASTEST / 2..=SLOWEST).contains(&interval) {
            return;
        }
        if self.intervals.len() == SAMPLES {
            self.intervals.pop_front();
        }
        self.intervals.push_back(interval);
        if self.intervals.len() >= ENOUGH {
            let mut sorted = Vec::from(self.intervals.clone());
            sorted.sort_unstable();
            // Late redraws only lengthen intervals, so a low quantile follows
            // the display rather than the slowest frames the app drew.
            self.measured = sorted[sorted.len() / 4].clamp(FASTEST, SLOWEST);
        }
    }

    /// Use the interval the platform reports for the window's screen.
    pub(super) fn report(&mut self, reported: Option<Duration>) {
        self.reported = reported.map(|period| period.clamp(FASTEST, SLOWEST));
    }

    /// Whether measuring is done: the rate is reported, enough redraws were
    /// timed since the last recalibration, or too many redraws gave nothing.
    pub(super) fn calibrated(&self) -> bool {
        self.unpaced
            || self.reported.is_some()
            || self.intervals.len() >= ENOUGH
            || self.redraws >= GIVE_UP
    }

    /// Stop measuring for good under the software renderer, which presents
    /// without waiting for vsync: its redraws take as long as drawing does,
    /// and slow ones would keep redrawing back to back until `GIVE_UP`. The
    /// estimate so far, or `DEFAULT`, stays in use.
    pub(super) fn unpaced(&mut self) {
        self.unpaced = true;
        self.intervals.clear();
        self.last = None;
    }

    /// Measure again, e.g. after the window moved to another display. The old
    /// estimate stays in use meanwhile.
    pub(super) fn recalibrate(&mut self) {
        self.intervals.clear();
        self.redraws = 0;
        self.last = None;
    }

    /// The window moved, perhaps onto another display of the same size and
    /// scale, which only measuring tells apart where none is reported.
    pub(super) fn moved(&mut self, at: Instant) {
        self.moved = Some(at);
    }

    /// Measure again once the window has stayed put `SETTLE` since it last
    /// moved.
    pub(super) fn settle(&mut self, now: Instant) {
        if self
            .moved
            .is_some_and(|at| now.saturating_duration_since(at) >= SETTLE)
        {
            self.moved = None;
            self.recalibrate();
        }
    }

    /// The display's refresh interval.
    pub(super) fn period(&self) -> Duration {
        self.reported.unwrap_or(self.measured)
    }
}

/// The refresh interval of the screen showing `window`, if the platform says.
#[cfg(target_os = "macos")]
pub(super) fn screen_period(window: &dyn iced::window::Window) -> Option<Duration> {
    use iced::window::raw_window_handle::RawWindowHandle;
    use objc2::{rc::Retained, runtime::NSObjectProtocol, sel};
    use objc2_app_kit::{NSScreen, NSView};
    let RawWindowHandle::AppKit(handle) = window.window_handle().ok()?.as_raw() else {
        return None;
    };
    // SAFETY: iced runs this on the main thread with the window alive, and the
    // handle's view is that window's content view.
    let view: &NSView = unsafe { handle.ns_view.cast().as_ref() };
    let screen: Retained<NSScreen> = view.window()?.screen()?;
    // Added in macOS 12; older systems keep measuring.
    if !screen.respondsToSelector(sel!(maximumFramesPerSecond)) {
        return None;
    }
    let fps = u32::try_from(screen.maximumFramesPerSecond()).ok()?;
    (fps > 0).then(|| Duration::from_secs(1) / fps)
}

#[cfg(not(target_os = "macos"))]
pub(super) fn screen_period(_window: &dyn iced::window::Window) -> Option<Duration> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(refresh: &mut Refresh, start: Instant, intervals: impl IntoIterator<Item = u64>) {
        let mut at = start;
        refresh.redraw(at);
        for micros in intervals {
            at += Duration::from_micros(micros);
            refresh.redraw(at);
        }
    }

    #[test]
    fn measures_the_display_through_late_frames() {
        let mut refresh = Refresh::default();
        assert_eq!(refresh.period(), DEFAULT);
        // 120 Hz with a third of the redraws a vsync late.
        let intervals = [8_333, 8_333, 16_667].repeat(8);
        feed(&mut refresh, Instant::now(), intervals);
        assert!(refresh.calibrated());
        assert_eq!(refresh.period(), Duration::from_micros(8_333));
    }

    #[test]
    fn ignores_idle_gaps_and_keeps_the_estimate_while_recalibrating() {
        let mut refresh = Refresh::default();
        let start = Instant::now();
        feed(&mut refresh, start, [6_944; 12]);
        assert_eq!(refresh.period(), Duration::from_micros(6_944));
        refresh.recalibrate();
        assert!(!refresh.calibrated());
        assert_eq!(refresh.period(), Duration::from_micros(6_944));
        // A pause between animations is not a refresh interval.
        feed(
            &mut refresh,
            start + Duration::from_secs(5),
            [500_000, 16_667],
        );
        assert_eq!(refresh.intervals.len(), 1);
        assert_eq!(refresh.period(), Duration::from_micros(6_944));
    }

    #[test]
    fn stops_measuring_a_display_it_cannot_time() {
        let mut refresh = Refresh::default();
        // 24 Hz is slower than any interval taken as a refresh.
        feed(&mut refresh, Instant::now(), [41_667; GIVE_UP as usize - 2]);
        assert!(!refresh.calibrated());
        refresh.redraw(Instant::now() + Duration::from_secs(10));
        assert!(refresh.calibrated());
        assert_eq!(refresh.period(), DEFAULT);
        refresh.recalibrate();
        assert!(!refresh.calibrated());
    }

    #[test]
    fn a_software_renderer_is_never_timed() {
        let mut refresh = Refresh::default();
        let start = Instant::now();
        feed(&mut refresh, start, [40_000; 4]);
        refresh.unpaced();
        assert!(refresh.calibrated());
        // Slow software redraws neither count nor keep redraws coming.
        feed(&mut refresh, start + Duration::from_secs(1), [20_000; 20]);
        assert_eq!(refresh.period(), DEFAULT);
        refresh.recalibrate();
        assert!(refresh.calibrated(), "moving the window changes nothing");
    }

    #[test]
    fn a_moved_window_is_measured_again_once_it_stays_put() {
        let mut refresh = Refresh::default();
        let start = Instant::now();
        feed(&mut refresh, start, [6_944; 12]);
        // A drag moves the window many times; it is measured after the last.
        for step in 0..10 {
            refresh.moved(start + Duration::from_millis(step * 100));
            refresh.settle(start + Duration::from_millis(step * 100 + 50));
            assert!(refresh.calibrated(), "still moving");
        }
        refresh.settle(start + Duration::from_millis(900 + 499));
        assert!(refresh.calibrated());
        refresh.settle(start + Duration::from_millis(900) + SETTLE);
        assert!(
            !refresh.calibrated(),
            "settled on what may be another display"
        );
        // Now on a 60 Hz display of the same size and scale.
        feed(&mut refresh, start + Duration::from_secs(2), [16_667; 12]);
        assert_eq!(refresh.period(), Duration::from_micros(16_667));
        refresh.settle(start + Duration::from_secs(5));
        assert!(refresh.calibrated(), "measured once per move");
    }

    #[test]
    fn a_reported_rate_wins_over_redraw_timing() {
        let mut refresh = Refresh::default();
        // Redraws that outrun a 60 Hz screen, as on macOS.
        feed(&mut refresh, Instant::now(), [8_000; 12]);
        refresh.report(Some(Duration::from_micros(16_667)));
        assert_eq!(refresh.period(), Duration::from_micros(16_667));
        refresh.recalibrate();
        assert!(refresh.calibrated());
        refresh.report(None);
        assert_eq!(refresh.period(), Duration::from_micros(8_000));
    }
}
