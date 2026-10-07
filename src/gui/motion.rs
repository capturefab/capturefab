//! Motion: every animation's timing, and the state behind the collapsible
//! panes, image mode and the controls that float over the stage.
use super::*;
use iced::animation::Easing;

pub(super) const PANEL: Duration = Duration::from_millis(200);
pub(super) const IMAGE: Duration = Duration::from_millis(240);
pub(super) const CONTROLS_IN: Duration = Duration::from_millis(140);
pub(super) const CONTROLS_OUT: Duration = Duration::from_millis(300);
/// How long the stage controls stay after the pointer last moved.
pub(super) const CONTROLS_IDLE: Duration = Duration::from_secs(2);
pub(super) const HOVER: Duration = Duration::from_millis(120);
pub(super) const RING: Duration = Duration::from_millis(160);
pub(super) const SHEET: Duration = Duration::from_millis(220);
pub(super) const SLIDE: Duration = Duration::from_millis(200);
pub(super) const SHUTTER: Duration = Duration::from_millis(320);
pub(super) const WELCOME: Duration = Duration::from_millis(400);
/// Below this window width the inspector floats over the stage instead of docking.
pub(super) const NARROW: f32 = 1100.0;

pub(super) fn reduce_motion() -> bool {
    static REDUCE: OnceLock<bool> = OnceLock::new();
    *REDUCE.get_or_init(|| {
        mac_default("com.apple.universalaccess", "reduceMotion").as_deref() == Some("1")
    })
}

pub(super) fn faded(value: bool, duration: Duration) -> Animation<bool> {
    Animation::new(value)
        .duration(duration)
        .easing(Easing::EaseOutCubic)
}

pub(super) fn eased(value: bool, duration: Duration) -> Animation<bool> {
    faded(
        value,
        if reduce_motion() {
            Duration::ZERO
        } else {
            duration
        },
    )
}

pub(super) fn rise(by: f32, shown: f32) -> f32 {
    if reduce_motion() {
        0.0
    } else {
        by * (1.0 - shown)
    }
}

/// Whether the stage controls should show: while the pointer is over them, or
/// for `CONTROLS_IDLE` after it last moved over the stage.
pub(super) fn controls_wanted(moved: Option<Instant>, now: Instant, hovering: bool) -> bool {
    hovering || moved.is_some_and(|moved| now.saturating_duration_since(moved) < CONTROLS_IDLE)
}

impl Workbench {
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

    pub(super) fn animating(&self) -> bool {
        let fading = self.notice.as_ref().is_some_and(|(_, error, at)| {
            let age = self.now.duration_since(*at);
            age < NOTICE_FADE
                || (!error && age > NOTICE_LIFE - NOTICE_FADE * 2 && age < NOTICE_LIFE)
        });
        fading
            || [
                &self.sheet,
                &self.activity_slide,
                &self.exposure_slide,
                &self.focus_slide,
                &self.shutter,
                &self.welcome,
                &self.sidebar_slide,
                &self.inspector_slide,
                &self.chrome,
                &self.controls,
                &self.tile_hover,
                &self.ring,
            ]
            .iter()
            .any(|animation| animation.is_animating(self.now))
    }

    pub(super) fn sync_animations(&mut self) {
        let now = self.now;
        let open = self.sheet_open();
        if open != self.sheet.value() {
            if open {
                self.sheet.go_mut(true, now);
            } else {
                self.sheet = eased(false, SHEET);
            }
        }
        let welcome = self.snapshot.connected.is_none() && !self.overview();
        if welcome != self.welcome.value() {
            if welcome {
                self.welcome.go_mut(true, now);
            } else {
                self.welcome = eased(false, WELCOME);
            }
        }
        let wanted = controls_wanted(self.pointer_moved, now, self.over_controls);
        if wanted != self.controls.value() {
            let duration = if wanted { CONTROLS_IN } else { CONTROLS_OUT };
            self.controls = faded(!wanted, duration).go(wanted, now);
        }
        let sidebar = self.sidebar_shown();
        let targets = [
            (self.logs_open, &mut self.activity_slide),
            (self.exposure_open, &mut self.exposure_slide),
            (self.focus.open, &mut self.focus_slide),
            (sidebar, &mut self.sidebar_slide),
            (!self.image_mode, &mut self.chrome),
        ];
        for (target, animation) in targets {
            if target != animation.value() {
                animation.go_mut(target, now);
            }
        }
        let inspector = self.inspector_shown();
        if inspector != self.inspector_slide.value() {
            self.inspector_slide.go_mut(inspector, now);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
