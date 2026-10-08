//! Motion: every animation's timing, the `Motion` and `Flashes` primitives,
//! the accessibility settings that shape them, and the state behind the
//! collapsible panes, image mode and the controls that float over the stage.
//!
//! Rules: animate state changes only, once, and settle. Entrances take
//! 120–280 ms and exits are faster; both ease out. Every `Motion` and
//! `Flashes` field joins the `registry!` list below, which is what keeps
//! redraws running while something moves and stops them once it settles.
//! Reduce Motion keeps fades and snaps movement.
use super::*;
use iced::animation::Easing;
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
pub(super) const HOVER: Duration = Duration::from_millis(120);
pub(super) const HOVER_OUT: Duration = Duration::from_millis(80);
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
pub(super) const NOTICE_OUT: Duration = Duration::from_millis(200);
/// How long a done or warning notice stays before it fades.
pub(super) const NOTICE_LIFE: Duration = Duration::from_secs(5);
/// How long a copy button reads as copied.
pub(super) const COPIED: Duration = Duration::from_millis(1500);
/// How long a capture control reads as saved.
pub(super) const SAVED: Duration = Duration::from_millis(1600);
/// How long `Flashes` remembers a hit, which bounds what `held` can answer.
pub(super) const FLASH_KEEP: Duration = Duration::from_secs(3);
/// How often the system's accessibility display settings are read again.
const ACCESSIBILITY_POLL: Duration = Duration::from_secs(2);
/// Below this window width the inspector floats over the stage instead of docking.
pub(super) const NARROW: f32 = 1100.0;

/// Reduce Motion and Reduce Transparency, read from the system; see `refresh_accessibility`.
static REDUCE_MOTION: AtomicBool = AtomicBool::new(false);
static REDUCE_TRANSPARENCY: AtomicBool = AtomicBool::new(false);

/// Read the system's Reduce Motion and Reduce Transparency settings again.
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
    }
}

/// Whether to replace movement with fades: `Kind::Move` motions snap.
pub(super) fn reduce_motion() -> bool {
    REDUCE_MOTION.load(Ordering::Relaxed)
}

/// Whether glass and scrims should be opaque.
#[allow(dead_code)] // adopted by the area packages
pub(super) fn reduce_transparency() -> bool {
    REDUCE_TRANSPARENCY.load(Ordering::Relaxed)
}

/// How far content still sits below its resting place as `shown` goes from
/// 0 to 1; nothing under Reduce Motion.
pub(super) fn rise(by: f32, shown: f32) -> f32 {
    if reduce_motion() {
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
        self.animation.interpolate_with(|value| value, now)
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
        self.animation.is_animating(now)
    }

    /// The longest any change can take.
    #[cfg(test)]
    pub(super) fn longest(&self) -> Duration {
        self.enter.max(self.exit)
    }

    /// How long going from `from` to `to` takes.
    fn span(&self, from: f32, to: f32, reduce: bool) -> Duration {
        if reduce && self.kind == Kind::Move {
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

#[allow(dead_code)] // adopted by the area packages
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

    /// Whether `key` was hit within `within`, at most `FLASH_KEEP` ago.
    pub(super) fn held<Q>(&self, key: &Q, now: Instant, within: Duration) -> bool
    where
        K: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        !self.at.is_empty()
            && self
                .at
                .get(key)
                .is_some_and(|at| now.saturating_duration_since(*at) < within)
    }

    pub(super) fn is_empty(&self) -> bool {
        self.at.is_empty()
    }

    /// Whether any hit is still fading.
    pub(super) fn animating(&self, now: Instant) -> bool {
        self.at
            .values()
            .any(|at| now.saturating_duration_since(*at) < self.duration)
    }

    /// Forget hits older than the duration and `FLASH_KEEP`.
    pub(super) fn prune(&mut self, now: Instant) {
        if self.at.is_empty() {
            return;
        }
        let keep = self.duration.max(FLASH_KEEP);
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

/// Lists every `Motion` and `Flashes` field once, generating the iterators
/// that `animating()`, `tick()` and the settle test walk. Paths may reach
/// into nested state, e.g. `focus.lock`.
macro_rules! registry {
    (
        motions: [$($($motion:ident).+),* $(,)?],
        flashes: [$($($flash:ident).+),* $(,)?] $(,)?
    ) => {
        /// Every `Motion`.
        pub(super) fn motions(&self) -> impl Iterator<Item = &Motion> {
            [$(&self.$($motion).+),*].into_iter()
        }

        #[cfg(test)]
        fn motions_mut(&mut self) -> impl Iterator<Item = &mut Motion> {
            [$(&mut self.$($motion).+),*].into_iter()
        }

        /// Every `Flashes` set.
        pub(super) fn flashes(&self) -> impl Iterator<Item = &dyn Flashing> {
            std::iter::empty::<&dyn Flashing>()
                $(.chain(std::iter::once(&self.$($flash).+ as &dyn Flashing)))*
        }

        fn flashes_mut(&mut self) -> impl Iterator<Item = &mut dyn Flashing> {
            std::iter::empty::<&mut dyn Flashing>()
                $(.chain(std::iter::once(&mut self.$($flash).+ as &mut dyn Flashing)))*
        }
    };
}

impl Workbench {
    registry! {
        motions: [
            sheet,
            activity_slide,
            exposure_slide,
            focus_slide,
            shutter,
            welcome,
            sidebar_slide,
            inspector_slide,
            chrome,
            controls,
            tile_hover,
            ring,
            notice_shown,
        ],
        flashes: [],
    }

    pub(super) fn sheet_open(&self) -> bool {
        self.help_open || self.capture_to.manager_open || self.record_to.manager_open
    }

    pub(super) fn sidebar_shown(&self) -> bool {
        self.sidebar_open && !self.image_mode
    }

    pub(super) fn inspector_docked(&self) -> bool {
        self.width >= NARROW
    }

    pub(super) fn inspector_shown(&self) -> bool {
        !self.image_mode
            && if self.inspector_docked() {
                self.inspector_open
            } else {
                self.inspector_peek
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

    /// The step a spinner shows: one of 8, advancing every 120 ms. The slow
    /// tick runs at its busy rate while anything is pending, so spinners
    /// step without per-frame redraws. Screenshots hold it at 0.
    #[allow(dead_code)] // adopted by the area packages
    pub(super) fn spin(&self) -> usize {
        if self.screenshot.is_some() {
            return 0;
        }
        (self.now.saturating_duration_since(self.born).as_millis() / 120 % 8) as usize
    }

    /// Point each motion at the state it shows.
    pub(super) fn sync_animations(&mut self) {
        let now = self.now;
        let sheet = self.sheet_open();
        let welcome = self.snapshot.connected.is_none() && !self.overview();
        let controls = controls_wanted(self.pointer_moved, now, self.over_controls);
        let sidebar = self.sidebar_shown();
        let inspector = self.inspector_shown();
        let targets = [
            (sheet, &mut self.sheet),
            (welcome, &mut self.welcome),
            (controls, &mut self.controls),
            (self.logs_open, &mut self.activity_slide),
            (self.exposure_open, &mut self.exposure_slide),
            (self.focus.open, &mut self.focus_slide),
            (sidebar, &mut self.sidebar_slide),
            (inspector, &mut self.inspector_slide),
            (!self.image_mode, &mut self.chrome),
        ];
        for (shown, motion) in targets {
            motion.show(shown, now);
        }
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
        assert!(flashes.held("Gain", start + ms(1500), ms(2000)));
        assert!(!flashes.held("Gain", start + ms(2500), ms(2000)));
        flashes.prune(start + ms(1000));
        assert!(!flashes.is_empty(), "kept for held()");
        flashes.prune(start + FLASH_KEEP);
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
