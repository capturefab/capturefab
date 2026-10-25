//! Motion: every animation's timing, the `Motion` and `Flashes` primitives,
//! the accessibility settings that shape them, and the state behind the
//! collapsible panes, image mode and the controls that float over the stage.
//!
//! Rules: animate state changes only, once, and settle. Entrances take
//! 120–280 ms and exits are faster; both ease out. Every `Motion` and
//! `Flashes` field joins a `registry!` list: the workbench's below, or the
//! one beside an area's own state in its file. That is what keeps redraws
//! running while something moves and stops them once it settles. Reduce
//! Motion keeps fades and snaps movement; efficiency mode snaps both, and
//! acknowledgements fade only as fast as other updates redraw them.
use super::*;
use iced::animation::Easing;
#[cfg(any(target_os = "linux", target_os = "windows"))]
use iced::futures::StreamExt;
use std::{borrow::Borrow, hash::Hash, sync::atomic::AtomicBool};

pub(super) const PANEL: Duration = Duration::from_millis(200);
pub(super) const PANEL_OUT: Duration = Duration::from_millis(160);
pub(super) const IMAGE: Duration = Duration::from_millis(240);
pub(super) const IMAGE_OUT: Duration = Duration::from_millis(200);
pub(super) const CONTROLS_IN: Duration = Duration::from_millis(140);
/// The stage controls hide on their own after the pointer rests, so they
/// leave slowly rather than snapping away mid-glance.
pub(super) const CONTROLS_OUT: Duration = Duration::from_millis(300);
/// How long the stage controls stay after the pointer last moved.
pub(super) const CONTROLS_IDLE: Duration = Duration::from_secs(2);
/// How long after waking the stage controls further pointer moves go
/// unheard. A rebuild, a tick at the latest, hears them again well inside
/// `CONTROLS_IDLE`, so the controls never hide under a moving pointer.
const POINTER_DEAF: Duration = Duration::from_millis(500);
pub(super) const RING: Duration = Duration::from_millis(160);
pub(super) const RING_OUT: Duration = Duration::from_millis(120);
pub(super) const SCOPE: Duration = Duration::from_millis(140);
pub(super) const SCOPE_OUT: Duration = Duration::from_millis(120);
pub(super) const SHEET: Duration = Duration::from_millis(220);
pub(super) const SHEET_OUT: Duration = Duration::from_millis(140);
pub(super) const SLIDE: Duration = Duration::from_millis(200);
pub(super) const SLIDE_OUT: Duration = Duration::from_millis(160);
pub(super) const SHUTTER: Duration = Duration::from_millis(320);
pub(super) const WELCOME: Duration = Duration::from_millis(400);
pub(super) const NOTICE_IN: Duration = Duration::from_millis(160);
pub(super) const NOTICE_OUT: Duration = Duration::from_millis(120);
/// How long a done or warning notice stays before it fades.
pub(super) const NOTICE_LIFE: Duration = Duration::from_secs(5);
/// How long a copy button reads as copied.
pub(super) const COPIED: Duration = Duration::from_millis(1500);
/// How long a capture control reads as saved.
pub(super) const SAVED: Duration = Duration::from_millis(1600);
/// How often the system's accessibility display settings are read again.
const ACCESSIBILITY_POLL: Duration = Duration::from_secs(2);
/// Below this window width the side panels float over the stage instead of docking.
pub(super) const NARROW: f32 = 1080.0;

/// Native accessibility preferences; macOS polls and other desktops stream changes.
static REDUCE_MOTION: AtomicBool = AtomicBool::new(false);
static REDUCE_TRANSPARENCY: AtomicBool = AtomicBool::new(false);
static INCREASE_CONTRAST: AtomicBool = AtomicBool::new(false);
static ALWAYS_SHOW_SCROLLBARS: AtomicBool = AtomicBool::new(false);

/// Read macOS's accessibility and scrollbar settings again.
/// Cheap; the workbench calls it at start and every couple of seconds, so
/// changing a setting applies without a restart.
pub(super) fn refresh_accessibility() {
    #[cfg(target_os = "macos")]
    {
        let workspace = objc2_app_kit::NSWorkspace::sharedWorkspace();
        REDUCE_MOTION.store(
            workspace.accessibilityDisplayShouldReduceMotion(),
            Ordering::Relaxed,
        );
        REDUCE_TRANSPARENCY.store(
            workspace.accessibilityDisplayShouldReduceTransparency(),
            Ordering::Relaxed,
        );
        INCREASE_CONTRAST.store(
            workspace.accessibilityDisplayShouldIncreaseContrast(),
            Ordering::Relaxed,
        );
        if let Some(main_thread) = objc2::MainThreadMarker::new() {
            ALWAYS_SHOW_SCROLLBARS.store(
                objc2_app_kit::NSScroller::preferredScrollerStyle(main_thread)
                    == objc2_app_kit::NSScrollerStyle::Legacy,
                Ordering::Relaxed,
            );
        }
    }
}

/// Listen to native preference changes without synchronous portal/WinRT
/// calls on the UI thread. The initial event also applies startup preferences.
pub(super) fn accessibility_subscription() -> Subscription<Message> {
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    {
        Subscription::run(|| {
            mundy::Preferences::stream(
                mundy::Interest::Contrast
                    | mundy::Interest::ReducedMotion
                    | mundy::Interest::ReducedTransparency
                    | mundy::Interest::ScrollbarVisibility,
            )
            .map(|preferences| {
                REDUCE_MOTION.store(preferences.reduced_motion.is_reduce(), Ordering::Relaxed);
                REDUCE_TRANSPARENCY.store(
                    preferences.reduced_transparency.is_reduce(),
                    Ordering::Relaxed,
                );
                INCREASE_CONTRAST.store(preferences.contrast.is_more(), Ordering::Relaxed);
                ALWAYS_SHOW_SCROLLBARS.store(
                    preferences.scrollbar_visibility.is_always(),
                    Ordering::Relaxed,
                );
                Message::Tick
            })
        })
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        Subscription::none()
    }
}

// Animation tests must choose a preference independently of the host desktop
// and of parallel tests that refresh native accessibility settings.
#[cfg(test)]
thread_local! {
    static TEST_REDUCE_MOTION: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub(super) struct TestMotion(Option<bool>);

#[cfg(test)]
impl Drop for TestMotion {
    fn drop(&mut self) {
        TEST_REDUCE_MOTION.set(self.0);
    }
}

#[cfg(test)]
pub(super) fn test_reduce_motion(reduce: bool) -> TestMotion {
    TestMotion(TEST_REDUCE_MOTION.replace(Some(reduce)))
}

/// Whether to replace movement with fades: `Kind::Move` motions snap.
pub(super) fn reduce_motion() -> bool {
    #[cfg(test)]
    if let Some(reduce) = TEST_REDUCE_MOTION.get() {
        return reduce;
    }
    REDUCE_MOTION.load(Ordering::Relaxed)
}

/// Whether glass should be opaque; `style::overlay` reads it.
pub(super) fn reduce_transparency() -> bool {
    REDUCE_TRANSPARENCY.load(Ordering::Relaxed)
}

pub(super) fn increase_contrast() -> bool {
    INCREASE_CONTRAST.load(Ordering::Relaxed)
}

pub(super) fn always_show_scrollbars() -> bool {
    ALWAYS_SHOW_SCROLLBARS.load(Ordering::Relaxed)
}

/// How far content still sits below its resting place as `shown` goes from
/// 0 to 1; nothing under Reduce Motion or in efficiency mode.
pub(super) fn rise(by: f32, shown: f32) -> f32 {
    if reduce_motion() || performance::efficient() {
        0.0
    } else {
        by * (1.0 - shown)
    }
}

/// What a `Motion` changes, which decides what Reduce Motion does to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    /// Opacity or colour: always runs, since a dissolve replaces movement.
    Fade,
    /// Position or size: snaps under Reduce Motion.
    Move,
}

/// A value easing towards a target: with the `enter` timing while it rises
/// and the `exit` timing while it falls. A new target takes over from
/// wherever the value is, in time proportional to the distance left, so
/// interrupting a change never jumps and a quick reversal is quick.
///
/// Targets are compared exactly; use 0, 1 or an index. Both directions ease
/// out (cubic), as everything else here does: the response starts at full
/// speed the moment something changes, and a retarget mid-way starts moving
/// at once instead of stalling as an ease-in would.
#[derive(Clone, Debug)]
pub(super) struct Motion {
    animation: Animation<f32>,
    enter: Duration,
    exit: Duration,
    kind: Kind,
}

impl Motion {
    pub(super) fn new(value: f32, enter: Duration, exit: Duration, kind: Kind) -> Self {
        Self {
            animation: Animation::new(value),
            enter,
            exit,
            kind,
        }
    }

    /// Head for `target` from the current value; nothing if already heading there.
    pub(super) fn go(&mut self, target: f32, now: Instant) {
        if self.animation.value() == target {
            return;
        }
        let from = self.get(now);
        self.animation = Animation::new(from)
            .duration(self.span(from, target, reduce_motion()))
            .easing(Easing::EaseOutCubic)
            .go(target, now);
    }

    /// Head for 1 when `shown`, else 0.
    pub(super) fn show(&mut self, shown: bool, now: Instant) {
        self.go(if shown { 1.0 } else { 0.0 }, now);
    }

    /// Play a one-shot change from `from` to `to`, e.g. a flash or a ring
    /// drawing in again for a new selection.
    pub(super) fn replay(&mut self, from: f32, to: f32, now: Instant) {
        self.set(from);
        self.go(to, now);
    }

    /// Jump to `value` without animating.
    pub(super) fn set(&mut self, value: f32) {
        self.animation = Animation::new(value);
    }

    /// The value at `now`.
    pub(super) fn get(&self, now: Instant) -> f32 {
        self.value(now, reduce_motion())
    }

    fn value(&self, now: Instant, reduced: bool) -> f32 {
        if self.snaps(reduced) {
            self.target()
        } else {
            self.animation.interpolate_with(|value| value, now)
        }
    }

    /// `a` at 0 to `b` at 1, at `now`.
    pub(super) fn lerp(&self, a: f32, b: f32, now: Instant) -> f32 {
        a + (b - a) * self.get(now)
    }

    /// Where the value is heading, or rests.
    pub(super) fn target(&self) -> f32 {
        self.animation.value()
    }

    pub(super) fn animating(&self, now: Instant) -> bool {
        self.active(now, reduce_motion())
    }

    fn active(&self, now: Instant, reduced: bool) -> bool {
        !self.snaps(reduced) && self.animation.is_animating(now)
    }

    /// Whether it jumps to its target: movement under Reduce Motion, and
    /// every motion in efficiency mode, which saves the redraws.
    fn snaps(&self, reduced: bool) -> bool {
        reduced && self.kind == Kind::Move || performance::efficient()
    }

    /// The longest any change can take.
    #[cfg(test)]
    pub(super) fn longest(&self) -> Duration {
        self.enter.max(self.exit)
    }

    /// How long going from `from` to `to` takes.
    fn span(&self, from: f32, to: f32, reduce: bool) -> Duration {
        if self.snaps(reduce) {
            return Duration::ZERO;
        }
        let base = if to > from { self.enter } else { self.exit };
        base.mul_f32((to - from).abs().min(1.0))
    }
}

/// Keyed one-shot acknowledgements (a row accepted, a tile captured): each
/// `hit` fades from 1 to 0 over the set's duration. Views may look up a key
/// per row: `level` returns at once while nothing was hit recently, so idle
/// rows cost nothing per frame. `tick()` prunes old hits.
#[derive(Clone, Debug)]
pub(super) struct Flashes<K> {
    at: HashMap<K, Instant>,
    duration: Duration,
}

impl<K: Eq + Hash> Flashes<K> {
    pub(super) fn new(duration: Duration) -> Self {
        Self {
            at: HashMap::new(),
            duration,
        }
    }

    pub(super) fn hit(&mut self, key: K, now: Instant) {
        self.at.insert(key, now);
    }

    /// 1 just after `key` was hit, easing out to 0 over the duration.
    pub(super) fn level<Q>(&self, key: &Q, now: Instant) -> f32
    where
        K: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        if self.at.is_empty() {
            return 0.0;
        }
        self.at.get(key).map_or(0.0, |at| {
            let progress = now.saturating_duration_since(*at).as_secs_f32()
                / self.duration.as_secs_f32().max(f32::EPSILON);
            (1.0 - progress.min(1.0)).powi(3)
        })
    }

    #[cfg(test)]
    pub(super) fn is_empty(&self) -> bool {
        self.at.is_empty()
    }

    /// Whether any hit is still fading, which keeps redraws coming; in
    /// efficiency mode other updates redraw it often enough.
    pub(super) fn animating(&self, now: Instant) -> bool {
        !performance::efficient()
            && self
                .at
                .values()
                .any(|at| now.saturating_duration_since(*at) < self.duration)
    }

    /// Forget hits that have faded. Hits set in the future, for a staggered
    /// start, stay until they have played.
    pub(super) fn prune(&mut self, now: Instant) {
        if self.at.is_empty() {
            return;
        }
        let keep = self.duration;
        self.at
            .retain(|_, at| now.saturating_duration_since(*at) < keep);
    }
}

/// A `Flashes` set as the registry sees it, whatever its key.
pub(super) trait Flashing {
    fn animating(&self, now: Instant) -> bool;
    fn prune(&mut self, now: Instant);
}

impl<K: Eq + Hash> Flashing for Flashes<K> {
    fn animating(&self, now: Instant) -> bool {
        Flashes::animating(self, now)
    }

    fn prune(&mut self, now: Instant) {
        Flashes::prune(self, now);
    }
}

/// Whether the stage controls should show: while the pointer is over them, or
/// for `CONTROLS_IDLE` after it last moved over the stage.
pub(super) fn controls_wanted(moved: Option<Instant>, now: Instant, hovering: bool) -> bool {
    hovering || moved.is_some_and(|moved| now.saturating_duration_since(moved) < CONTROLS_IDLE)
}

/// Lists every `Motion` and `Flashes` field of a struct once, generating
/// the iterators that `animating()`, `tick()` and the settle test walk.
/// Paths may reach into nested state, e.g. `focus.lock`. `areas` names
/// fields whose own registries join this one. An area registers what it adds
/// beside its state, in its own file:
///
/// ```ignore
/// impl ChromeState {
///     motion::registry! {
///         motions: [live],
///         flashes: [],
///     }
/// }
/// ```
macro_rules! registry {
    (
        motions: [$($($motion:ident).+),* $(,)?],
        flashes: [$($($flash:ident).+),* $(,)?]
        $(, areas: [$($area:ident),* $(,)?])? $(,)?
    ) => {
        /// Every `Motion` registered here.
        pub(super) fn motions(&self) -> impl Iterator<Item = &$crate::gui::motion::Motion> {
            std::iter::empty::<&$crate::gui::motion::Motion>()
                $(.chain(std::iter::once(&self.$($motion).+)))*
                $($(.chain(self.$area.motions()))*)?
        }

        #[cfg(test)]
        pub(super) fn motions_mut(
            &mut self,
        ) -> impl Iterator<Item = &mut $crate::gui::motion::Motion> {
            std::iter::empty::<&mut $crate::gui::motion::Motion>()
                $(.chain(std::iter::once(&mut self.$($motion).+)))*
                $($(.chain(self.$area.motions_mut()))*)?
        }

        /// Every `Flashes` set registered here.
        pub(super) fn flashes(&self) -> impl Iterator<Item = &dyn $crate::gui::motion::Flashing> {
            std::iter::empty::<&dyn $crate::gui::motion::Flashing>()
                $(.chain(std::iter::once(
                    &self.$($flash).+ as &dyn $crate::gui::motion::Flashing,
                )))*
                $($(.chain(self.$area.flashes()))*)?
        }

        pub(super) fn flashes_mut(
            &mut self,
        ) -> impl Iterator<Item = &mut dyn $crate::gui::motion::Flashing> {
            std::iter::empty::<&mut dyn $crate::gui::motion::Flashing>()
                $(.chain(std::iter::once(
                    &mut self.$($flash).+ as &mut dyn $crate::gui::motion::Flashing,
                )))*
                $($(.chain(self.$area.flashes_mut()))*)?
        }
    };
}
pub(super) use registry;

impl Workbench {
    // Shared motions only; areas register theirs beside their own state.
    registry! {
        motions: [
            activity_slide,
            exposure_slide,
            focus_slide,
            shutter,
            welcome,
            inspector_slide,
            sidebar_slide,
            bars,
            controls,
            notice_shown,
        ],
        flashes: [],
        areas: [chrome, side, stage_ui, inspect, sheets, capture_to, record_to],
    }

    pub(super) fn sheet_open(&self) -> bool {
        self.sign_in.is_some()
            || self.help_open
            || self.assistant.quick.open
            || self.network_open
            || self.analytics.open
            || self.resources.open
            || self.updates.open
            || self.setups.open
            || (self.settings_open && self.settings_window.is_none())
            || self.capture_to.manager_open
            || self.record_to.manager_open
    }

    pub(super) fn panels_docked(&self) -> bool {
        self.width >= NARROW
    }

    pub(super) fn inspector_shown(&self) -> bool {
        !self.image_mode
            && self.snapshot.connected.is_some()
            && if self.panels_docked() {
                self.inspector_open
            } else {
                self.inspector_peek
            }
    }

    pub(super) fn sidebar_shown(&self) -> bool {
        !self.image_mode
            && (self.snapshot.connected.is_some() || self.sidebar_summoned)
            && if self.panels_docked() {
                self.sidebar_open
            } else {
                self.sidebar_peek
            }
    }

    /// Open the inspector; in a narrow window it floats, and only one
    /// panel floats at a time.
    pub(super) fn show_inspector(&mut self) {
        self.image_mode = false;
        if self.panels_docked() {
            self.inspector_open = true;
        } else {
            self.inspector_peek = true;
            self.sidebar_peek = false;
        }
    }

    /// Open the sidebar, as `show_inspector` opens the inspector.
    pub(super) fn show_sidebar(&mut self) {
        self.image_mode = false;
        self.sidebar_summoned = self.snapshot.connected.is_none();
        if self.panels_docked() {
            self.sidebar_open = true;
        } else {
            self.sidebar_peek = true;
            self.inspector_peek = false;
        }
    }

    /// Whether anything moves, which keeps redraws coming every display frame.
    pub(super) fn animating(&self) -> bool {
        let now = self.now;
        self.motions().any(|motion| motion.animating(now))
            || self.flashes().any(|flashes| flashes.animating(now))
    }

    /// Drop acknowledgements nothing reads any more; from `tick()`.
    pub(super) fn prune_flashes(&mut self) {
        let now = self.now;
        for flashes in self.flashes_mut() {
            flashes.prune(now);
        }
    }

    /// Read the accessibility settings again every couple of seconds; from `tick()`.
    pub(super) fn poll_accessibility(&mut self) {
        if self.now.saturating_duration_since(self.accessibility_read) >= ACCESSIBILITY_POLL {
            self.accessibility_read = self.now;
            refresh_accessibility();
        }
    }

    /// Show the stage controls for `CONTROLS_IDLE`, as moving the pointer does.
    pub(super) fn wake_controls(&mut self) {
        self.pointer_moved = Some(self.now);
    }

    /// Whether the stage hears the pointer move: not just after it woke
    /// the controls, so a moving pointer rebuilds the window a few times a
    /// second rather than with every move.
    pub(super) fn pointer_heard(&self) -> bool {
        self.pointer_moved
            .is_none_or(|moved| self.now.saturating_duration_since(moved) >= POINTER_DEAF)
    }

    /// The step a spinner shows: one of 8, advancing every 120 ms. The slow
    /// tick runs at its busy rate while anything is pending, so spinners
    /// step without per-frame redraws. Screenshots hold it at 0.
    pub(super) fn spin(&self) -> usize {
        if self.screenshot.is_some() {
            return 0;
        }
        (self.now.saturating_duration_since(self.born).as_millis() / 120 % 8) as usize
    }

    /// Point each motion at the state it shows, then let each area point its own.
    pub(super) fn sync_animations(&mut self) {
        let now = self.now;
        let welcome = self.snapshot.connected.is_none();
        let controls = controls_wanted(self.pointer_moved, now, self.over_controls);
        let inspector = self.inspector_shown();
        let sidebar = self.sidebar_shown();
        let targets = [
            (welcome, &mut self.welcome),
            (controls, &mut self.controls),
            (self.logs_open, &mut self.activity_slide),
            (self.exposure_open, &mut self.exposure_slide),
            (self.focus.open, &mut self.focus_slide),
            (inspector, &mut self.inspector_slide),
            (sidebar, &mut self.sidebar_slide),
            (!self.image_mode, &mut self.bars),
        ];
        for (shown, motion) in targets {
            motion.show(shown, now);
        }
        self.sync_chrome();
        self.sync_side();
        self.sync_stage();
        self.sync_inspector();
        self.sync_sheets();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    #[test]
    fn stage_controls_hide_after_the_pointer_rests() {
        let moved = Instant::now();
        assert!(!controls_wanted(None, moved, false));
        assert!(controls_wanted(None, moved, true));
        assert!(controls_wanted(
            Some(moved),
            moved + CONTROLS_IDLE / 2,
            false
        ));
        assert!(!controls_wanted(Some(moved), moved + CONTROLS_IDLE, false));
        assert!(controls_wanted(
            Some(moved),
            moved + CONTROLS_IDLE * 3,
            true
        ));
    }

    #[test]
    fn a_moving_pointer_keeps_the_controls_up_with_few_messages() {
        let mut bench = Workbench::new(SessionHandle::new(), "test".into(), true, None);
        let start = bench.now;
        assert!(bench.pointer_heard());
        bench.wake_controls();
        assert!(!bench.pointer_heard(), "the next moves are not needed");
        // Ticks rebuild the view at least once a second: by then it hears
        // the pointer again, long before the controls would hide.
        bench.now = start + POINTER_DEAF;
        assert!(bench.pointer_heard());
        assert!(POINTER_DEAF + Duration::from_secs(1) < CONTROLS_IDLE);
        assert!(controls_wanted(bench.pointer_moved, bench.now, false));
    }

    #[test]
    fn motion_eases_in_and_out_with_its_own_timings() {
        let start = Instant::now();
        let mut motion = Motion::new(0.0, ms(200), ms(100), Kind::Fade);
        motion.go(1.0, start);
        assert_eq!(motion.target(), 1.0);
        assert!(motion.animating(start + ms(100)));
        assert!(motion.get(start + ms(100)) > 0.5, "eases out");
        assert!(!motion.animating(start + ms(200)));
        assert_eq!(motion.get(start + ms(200)), 1.0);
        assert_eq!(motion.lerp(10.0, 20.0, start + ms(200)), 20.0);
        let later = start + ms(1000);
        motion.go(0.0, later);
        assert!(motion.animating(later + ms(99)));
        assert!(!motion.animating(later + ms(100)));
        assert_eq!(motion.get(later + ms(100)), 0.0);
    }

    #[test]
    fn interrupted_motion_continues_from_where_it_is() {
        let start = Instant::now();
        let mut motion = Motion::new(0.0, ms(200), ms(100), Kind::Fade);
        motion.go(1.0, start);
        let turn = start + ms(100);
        let reached = motion.get(turn);
        assert!(reached > 0.0 && reached < 1.0);
        motion.go(0.0, turn);
        assert!((motion.get(turn) - reached).abs() < 1e-4, "no jump");
        // The way back covers `reached` of the exit's full distance.
        let back = ms(100).mul_f32(reached);
        assert!(motion.animating(turn + back - ms(2)));
        assert!(!motion.animating(turn + back + ms(1)));
        assert_eq!(motion.get(turn + back + ms(1)), 0.0);
        // Heading where it already heads changes nothing.
        let before = motion.get(turn + ms(10));
        motion.go(0.0, turn + ms(10));
        assert_eq!(motion.get(turn + ms(10)), before);
    }

    #[test]
    fn enabling_reduce_motion_settles_movement_already_in_flight() {
        let start = Instant::now();
        for kind in [Kind::Move, Kind::Fade] {
            let mut motion = Motion::new(0.0, ms(200), ms(100), kind);
            motion.animation = Animation::new(0.0).duration(ms(200)).go(1.0, start);
            let halfway = start + ms(100);
            assert_eq!(motion.value(halfway, false), 0.5);
            assert!(motion.active(halfway, false));
            if kind == Kind::Move {
                assert_eq!(motion.value(halfway, true), 1.0);
                assert!(
                    !motion.active(halfway, true),
                    "no redraws for hidden movement"
                );
            } else {
                assert_eq!(motion.value(halfway, true), 0.5);
                assert!(motion.active(halfway, true), "the replacement fade remains");
            }
        }
    }

    #[test]
    fn reduce_motion_snaps_movement_but_keeps_fades() {
        let slide = Motion::new(0.0, ms(200), ms(150), Kind::Move);
        let fade = Motion::new(0.0, ms(200), ms(150), Kind::Fade);
        assert_eq!(slide.span(0.0, 1.0, true), Duration::ZERO);
        assert_eq!(slide.span(0.0, 1.0, false), ms(200));
        assert_eq!(fade.span(0.0, 1.0, true), ms(200));
        assert_eq!(fade.span(1.0, 0.0, true), ms(150));
        // Index targets further than one step take the full time, no longer.
        assert_eq!(slide.span(0.0, 2.0, false), ms(200));
        assert_eq!(slide.span(0.5, 0.0, false), ms(75));
    }

    #[test]
    fn efficiency_mode_snaps_fades_too_and_lets_flashes_rest() {
        let start = Instant::now();
        let _efficient = performance::process::force(true);
        for kind in [Kind::Move, Kind::Fade] {
            let mut motion = Motion::new(0.0, ms(200), ms(100), kind);
            motion.go(1.0, start);
            assert_eq!(motion.get(start + ms(50)), 1.0, "{kind:?} snaps");
            assert!(!motion.animating(start + ms(50)));
        }
        assert_eq!(rise(6.0, 0.5), 0.0);
        let mut flashes = Flashes::new(ms(400));
        flashes.hit("Gain".to_string(), start);
        assert!(!flashes.animating(start + ms(100)), "no redraws of its own");
        assert!(flashes.level("Gain", start + ms(100)) > 0.0, "still shown");
    }

    #[test]
    fn one_shot_replays_from_its_start() {
        let start = Instant::now();
        let mut flash = Motion::new(0.0, SHUTTER, SHUTTER, Kind::Fade);
        flash.replay(1.0, 0.0, start);
        assert_eq!(flash.get(start), 1.0);
        assert!(flash.animating(start + SHUTTER / 2));
        assert!(!flash.animating(start + SHUTTER));
        assert_eq!(flash.get(start + SHUTTER), 0.0);
    }

    #[test]
    fn flashes_fade_per_key_and_prune() {
        let start = Instant::now();
        let mut flashes = Flashes::new(ms(400));
        assert_eq!(flashes.level("Gain", start), 0.0);
        assert!(!flashes.animating(start));
        flashes.hit("Gain".to_string(), start);
        assert_eq!(flashes.level("Gain", start), 1.0);
        assert_eq!(flashes.level("ExposureTime", start), 0.0);
        let mid = flashes.level("Gain", start + ms(200));
        assert!(mid > 0.0 && mid < 0.5, "eases out: {mid}");
        assert!(flashes.animating(start + ms(399)));
        assert!(!flashes.animating(start + ms(400)));
        assert!(
            Flashing::animating(&flashes, start + ms(399)),
            "as registered"
        );
        assert_eq!(flashes.level("Gain", start + ms(400)), 0.0);
        flashes.prune(start + ms(399));
        assert!(!flashes.is_empty(), "kept while fading");
        flashes.hit("Gain".to_string(), start + ms(1000));
        flashes.prune(start + ms(500));
        assert!(!flashes.is_empty(), "a staggered hit waits for its start");
        assert_eq!(flashes.level("Gain", start + ms(500)), 1.0);
        flashes.prune(start + ms(1400));
        assert!(flashes.is_empty());
    }

    #[test]
    fn every_motion_settles() {
        let mut bench = Workbench::new(SessionHandle::new(), "test".into(), true, None);
        let now = bench.now;
        let longest = bench.motions().map(Motion::longest).max().unwrap();
        assert!(longest > Duration::ZERO);
        for round in 0..2 {
            let start = now + longest * 2 * round;
            bench.now = start;
            for motion in bench.motions_mut() {
                let target = if motion.target() > 0.5 { 0.0 } else { 1.0 };
                motion.go(target, start);
            }
            bench.now = start + ms(1);
            assert!(bench.animating(), "round {round} starts moving");
            bench.now = start + longest;
            assert!(!bench.animating(), "round {round} settles");
        }
        assert_eq!(bench.flashes().count(), bench.flashes_mut().count());
    }
}
