//! Performance: the efficiency setting as the desktop applies it, what the
//! desktop tells Automatic, and whether each shown camera's preview keeps up.
//! Captures and recordings keep every frame, whatever this decides.
use super::*;
use crate::efficiency::{Mode, Resolved};

/// The renderer is judged after this many draws: wgpu builds the preview
/// pipeline on the first draw that holds the probe.
const RENDERER_DRAWS: u64 = 3;
/// What Automatic hears with no GPU pipeline.
const SOFTWARE: &str = "software rendering";
/// What Automatic hears when a preview fell behind.
const OVERLOAD: &str = "the preview fell behind";
/// How long a preview must read behind, or caught up, before it counts.
const HOLD: Duration = Duration::from_secs(3);
/// A preview drawing less than this share of the frames it could is behind;
/// one drawing at least `CAUGHT_UP` of them has caught up.
const BEHIND: f64 = 0.8;
const CAUGHT_UP: f64 = 0.9;
/// Below this many frames a second, a second's count says too little.
const SLOWEST_JUDGED: f64 = 5.0;
/// Drawn frames are counted over windows this long.
const WINDOW: Duration = Duration::from_secs(1);

/// The process-wide efficiency state. Unit tests run beside others that read
/// it, so under test each thread keeps its own, resolved the same way from a
/// host that is never small.
#[cfg(not(test))]
mod process {
    use crate::efficiency::{self, Mode, Resolved};
    pub(super) fn efficient() -> bool {
        efficiency::efficient()
    }
    pub(super) fn current() -> Resolved {
        efficiency::current()
    }
    pub(super) fn set(mode: Mode) {
        efficiency::set(mode);
    }
    pub(super) fn hint(reason: &'static str) {
        efficiency::hint(reason);
    }
    pub(super) fn clear_hint(reason: &'static str) {
        efficiency::clear_hint(reason);
    }
    /// Without the preview pipeline, iced's software renderer draws.
    pub(super) fn software() -> bool {
        !super::gpu::available()
    }
}

#[cfg(test)]
pub(super) mod process {
    use crate::efficiency::{Mode, Resolved};
    use std::cell::RefCell;

    #[derive(Clone, Default)]
    struct State {
        preference: Mode,
        hints: Vec<&'static str>,
        forced: Option<bool>,
        software: bool,
    }
    thread_local! { static STATE: RefCell<State> = RefCell::default(); }

    pub(in super::super) fn efficient() -> bool {
        STATE
            .with_borrow(|state| state.forced)
            .unwrap_or_else(|| current().efficient)
    }
    pub(in super::super) fn current() -> Resolved {
        STATE.with_borrow(|state| match state.preference {
            Mode::Auto => Resolved {
                setting: Mode::Auto,
                efficient: !state.hints.is_empty(),
                reasons: state.hints.iter().map(|hint| hint.to_string()).collect(),
            },
            mode => Resolved {
                setting: mode,
                efficient: mode == Mode::Efficient,
                reasons: vec!["set in the desktop settings".into()],
            },
        })
    }
    pub(in super::super) fn set(mode: Mode) {
        STATE.with_borrow_mut(|state| state.preference = mode);
    }
    pub(in super::super) fn hint(reason: &'static str) {
        STATE.with_borrow_mut(|state| {
            if !state.hints.contains(&reason) {
                state.hints.push(reason);
            }
        });
    }
    pub(in super::super) fn clear_hint(reason: &'static str) {
        STATE.with_borrow_mut(|state| state.hints.retain(|hint| *hint != reason));
    }
    /// Start a workbench's thread afresh, with only its preference.
    pub(in super::super) fn reset(mode: Mode) {
        STATE.with_borrow_mut(|state| {
            state.preference = mode;
            state.hints.clear();
        });
    }
    pub(in super::super) fn software() -> bool {
        STATE.with_borrow(|state| state.software)
    }
    pub(in super::super) fn render_in_software(software: bool) {
        STATE.with_borrow_mut(|state| state.software = software);
    }

    /// Holds `efficient()` at an answer until dropped.
    pub(in super::super) struct Forced(Option<bool>);
    impl Drop for Forced {
        fn drop(&mut self) {
            STATE.with_borrow_mut(|state| state.forced = self.0);
        }
    }
    pub(in super::super) fn force(efficient: bool) -> Forced {
        Forced(STATE.with_borrow_mut(|state| state.forced.replace(efficient)))
    }
}

/// Whether efficiency mode is on: cheap enough for every frame.
pub(super) fn efficient() -> bool {
    process::efficient()
}

/// Start the desktop's part of the efficiency setting: its preference.
pub(super) fn start(mode: Mode) {
    #[cfg(test)]
    process::reset(mode);
    #[cfg(not(test))]
    process::set(mode);
}

/// What the Performance setting's segments say.
fn label(mode: Mode) -> &'static str {
    match mode {
        Mode::Auto => "Automatic",
        Mode::Smooth => "Smooth",
        Mode::Efficient => "Efficient",
    }
}

fn about(mode: Mode) -> &'static str {
    match mode {
        Mode::Auto => {
            "Use Efficient on small or low-power computers, and when this one falls behind"
        }
        Mode::Smooth => "Draw camera frames as often as the display refreshes",
        Mode::Efficient => "Draw fewer camera frames and skip animations",
    }
}

/// The line under the Performance setting: what Automatic chose and why, or
/// what overrides the choice. `None` when the choice simply stands.
fn note(chosen: Mode, resolved: &Resolved) -> Option<String> {
    let mode = if resolved.efficient {
        "Efficient"
    } else {
        "Smooth"
    };
    match resolved.reasons.first() {
        Some(source) if source.starts_with("set by ") => {
            Some(format!("{mode} is {source}, which overrides this setting."))
        }
        _ if resolved.setting != chosen => Some(format!(
            "The command line or {} overrides this setting; {mode} is in use.",
            crate::efficiency::ENV
        )),
        _ if chosen != Mode::Auto => None,
        _ if resolved.efficient => Some(format!(
            "Automatic chose Efficient: {}.",
            resolved.reasons.join(", ")
        )),
        _ => Some("Automatic chose Smooth for this computer.".into()),
    }
}

/// Whether one shown camera's frames reach the screen as fast as they could.
#[derive(Default)]
struct Keeping {
    /// New frames drawn in the current window.
    counted: u32,
    /// Frames a second drawn over the last whole window.
    rate: f64,
    behind: bool,
    /// Since when the rate has read the other way from `behind`.
    turning: Option<Instant>,
}

impl Keeping {
    /// Turn `behind` on once the rate stays under `BEHIND` of `possible`
    /// for `HOLD`, and off once it stays at `CAUGHT_UP` or more as long.
    fn judge(&mut self, possible: f64, now: Instant) {
        let turning = if self.behind {
            self.rate >= possible * CAUGHT_UP
        } else {
            self.rate < possible * BEHIND
        };
        if !turning {
            self.turning = None;
            return;
        }
        let since = *self.turning.get_or_insert(now);
        if now.saturating_duration_since(since) >= HOLD {
            self.behind = !self.behind;
            self.turning = None;
        }
    }
}

/// The desktop's performance state, beside the workbench's.
#[derive(Default)]
pub(super) struct PerformanceState {
    /// The preference last handed to the efficiency setting.
    applied: Option<Mode>,
    /// What the setting resolved to at the last tick, for Settings.
    pub(super) resolved: Option<Resolved>,
    renderer_judged: bool,
    /// Automatic has heard that a preview fell behind.
    overloaded: bool,
    /// The notice saying so has shown, once a run.
    noticed: bool,
    /// Each shown camera's preview, by camera ID.
    cameras: HashMap<String, Keeping>,
    /// When the current counting window started.
    since: Option<Instant>,
}

impl Workbench {
    /// Hand a changed Performance preference, chosen here or by undo, to
    /// the efficiency setting. A choice also starts Automatic afresh.
    pub(super) fn sync_performance(&mut self) {
        let state = &mut self.performance_state;
        if state.applied == Some(self.performance) {
            return;
        }
        if state.applied.is_some() {
            process::clear_hint(OVERLOAD);
            state.overloaded = false;
            process::set(self.performance);
        }
        state.applied = Some(self.performance);
        state.resolved = Some(process::current());
    }

    /// The performance bookkeeping on the slow tick: judge the renderer,
    /// count what each preview drew and tell Automatic when it fell behind.
    pub(super) fn tick_performance(&mut self) {
        self.judge_renderer();
        self.count_presented();
        self.hint_overload();
        self.performance_state.resolved = Some(process::current());
    }

    /// Once the main window has drawn a few times: without a GPU pipeline
    /// the software renderer draws, so Automatic hears it, and redraw timing
    /// measures the drawing, not the display.
    fn judge_renderer(&mut self) {
        let state = &mut self.performance_state;
        if state.renderer_judged || self.draws.last().1 < RENDERER_DRAWS {
            return;
        }
        state.renderer_judged = true;
        if process::software() {
            process::hint(SOFTWARE);
            self.refresh.unpaced();
        }
    }

    /// Add the frames each shown camera's view drew, and every `WINDOW`
    /// judge whether its preview keeps up: only while the main window draws
    /// and has the keyboard, since a background window may draw less.
    fn count_presented(&mut self) {
        let now = self.now;
        let cameras_shown = self.cameras_shown();
        let shown: Vec<(String, f64)> = self
            .snapshot
            .cameras
            .iter()
            .filter(|camera| camera.streaming && cameras_shown.contains(&camera.info.id.as_str()))
            .map(|camera| (camera.info.id.clone(), camera.fps))
            .collect();
        let showing = |camera: &str| shown.iter().any(|(id, _)| id == camera);
        self.draws.retain(showing);
        let state = &mut self.performance_state;
        state.cameras.retain(|camera, _| showing(camera));
        for (camera, count) in self.draws.take_presented() {
            state.cameras.entry(camera).or_default().counted += count;
        }
        let since = *state.since.get_or_insert(now);
        let elapsed = now.saturating_duration_since(since);
        if elapsed < WINDOW {
            return;
        }
        state.since = Some(now);
        let judging = self.screenshot.is_none()
            && !self.frames_paused()
            && self.key_window == Some(self.main_window);
        let cap = 1.0 / self.frame_interval().as_secs_f64();
        for (id, fps) in shown {
            let keeping = self.performance_state.cameras.entry(id).or_default();
            keeping.rate = f64::from(std::mem::take(&mut keeping.counted)) / elapsed.as_secs_f64();
            let possible = fps.min(cap);
            if judging && possible >= SLOWEST_JUDGED {
                keeping.judge(possible, now);
            } else {
                keeping.turning = None;
            }
        }
    }

    /// Tell Automatic once that a preview fell behind; say so the first
    /// time that makes the preview efficient.
    fn hint_overload(&mut self) {
        let state = &mut self.performance_state;
        let auto = state
            .resolved
            .as_ref()
            .is_some_and(|resolved| resolved.setting == Mode::Auto);
        if state.overloaded || !auto || !state.cameras.values().any(|keeping| keeping.behind) {
            return;
        }
        state.overloaded = true;
        let before = efficient();
        process::hint(OVERLOAD);
        if !before && efficient() && !std::mem::replace(&mut state.noticed, true) {
            self.set_notice(
                "Switched to Efficient preview because this computer was falling behind",
                Some("Captures and recordings keep every frame. Settings › Performance can choose Smooth again.".into()),
                Level::Done,
            );
        }
    }

    /// The frames a second camera `id`'s preview draws and the camera sends,
    /// while it draws clearly fewer than it could.
    pub(super) fn preview_behind(&self, id: &str) -> Option<(f64, f64)> {
        let keeping = self.performance_state.cameras.get(id)?;
        let camera = self.snapshot.cameras.iter().find(|c| c.info.id == id)?;
        keeping.behind.then_some((keeping.rate, camera.fps))
    }

    /// "Preview 9 of 30 fps", for the stage details and the title pill.
    pub(super) fn preview_figure(&self, id: &str) -> Option<String> {
        let (drawn, sent) = self.preview_behind(id)?;
        Some(format!("Preview {drawn:.0} of {sent:.0} fps"))
    }

    /// What the preview figure means, and what to do about it.
    pub(super) fn preview_about(&self) -> String {
        let mut about = "This computer is drawing fewer frames than the camera sends. Captures and recordings still get every frame.".to_owned();
        about.push_str(if efficient() {
            " Efficient preview is on."
        } else {
            " Use Efficient preview to lower the load."
        });
        about
    }

    /// The neutral chip with the preview figure, whose press turns on
    /// Efficient preview unless it is on; `shown` fades it with the controls.
    pub(super) fn preview_chip(&self, id: &str, shown: f32) -> Option<Element<'_, Message>> {
        let figure = self.preview_figure(id)?;
        let stage = style::stage_palette(self.key_window == Some(self.main_window));
        let switch = !efficient();
        let label = text(figure.clone())
            .size(style::SMALL)
            .color(fade(stage.secondary, shown))
            .wrapping(text::Wrapping::None);
        let chip = button(label)
            .name(if switch {
                format!("{figure}. Use Efficient preview")
            } else {
                figure
            })
            .padding([0, 5])
            .style(style::on_glass(false, shown))
            .on_press_maybe(switch.then_some(Message::Performance(Mode::Efficient)));
        Some(tip_above(chip, self.preview_about()))
    }

    /// The Performance section of Settings.
    pub(super) fn performance_section(&self, p: &'static Palette) -> Element<'_, Message> {
        let index = Mode::ALL
            .iter()
            .position(|mode| *mode == self.performance)
            .unwrap_or_default();
        let mut section = iced::widget::column![
            heading("Performance", p),
            segmented(
                "Performance",
                Mode::ALL
                    .into_iter()
                    .map(|mode| (
                        label(mode),
                        Some(Message::Performance(mode)),
                        about(mode).into()
                    ))
                    .collect(),
                index,
                index as f32,
            ),
            text(
                "Efficient draws up to 15 camera frames a second, 5 in the overview, and skips animations. Captures and recordings keep every frame."
            )
            .size(style::SMALL)
            .color(p.secondary),
        ]
        .spacing(9);
        if let Some(note) = self
            .performance_state
            .resolved
            .as_ref()
            .and_then(|resolved| note(self.performance, resolved))
        {
            section = section.push(text(note).size(style::SMALL).color(p.secondary));
        }
        section.into()
    }

    /// Take a screenshot scene word for performance: `performance-hint`
    /// (Automatic heard of software rendering) and `preview-behind` (the
    /// selected camera's preview draws 9 frames a second of its 30, with the
    /// stage controls up to show it).
    pub(super) fn scene_performance(&mut self, word: &str, late: bool) -> bool {
        match (word, late) {
            ("performance-hint", false) => {
                process::hint(SOFTWARE);
                self.performance_state.resolved = Some(process::current());
            }
            ("preview-behind", false) => {
                self.over_controls = true;
                return false;
            }
            ("preview-behind", true) => {
                let Some(id) = self.snapshot.active_camera.clone() else {
                    return true;
                };
                let keeping = self
                    .performance_state
                    .cameras
                    .entry(id.clone())
                    .or_default();
                keeping.behind = true;
                keeping.rate = 9.0;
                for camera in self.snapshot.cameras.iter_mut().filter(|c| c.info.id == id) {
                    camera.fps = 30.0;
                }
            }
            _ => return false,
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    fn resolved(setting: Mode, efficient: bool, reasons: &[&str]) -> Resolved {
        Resolved {
            setting,
            efficient,
            reasons: reasons.iter().map(|reason| reason.to_string()).collect(),
        }
    }

    #[test]
    fn the_note_says_what_automatic_chose_or_what_overrides_the_choice() {
        assert_eq!(
            note(Mode::Auto, &resolved(Mode::Auto, false, &[])).unwrap(),
            "Automatic chose Smooth for this computer."
        );
        assert_eq!(
            note(
                Mode::Auto,
                &resolved(Mode::Auto, true, &["2 CPU cores", "software rendering"])
            )
            .unwrap(),
            "Automatic chose Efficient: 2 CPU cores, software rendering."
        );
        let chosen = resolved(Mode::Smooth, false, &["set in the desktop settings"]);
        assert_eq!(note(Mode::Smooth, &chosen), None);
        let environment = resolved(
            Mode::Efficient,
            true,
            &["set by CAPTUREFAB_EFFICIENCY=efficient"],
        );
        assert_eq!(
            note(Mode::Smooth, &environment).unwrap(),
            "Efficient is set by CAPTUREFAB_EFFICIENCY=efficient, which overrides this setting."
        );
        assert_eq!(
            note(Mode::Smooth, &resolved(Mode::Auto, false, &[])).unwrap(),
            "The command line or CAPTUREFAB_EFFICIENCY overrides this setting; Smooth is in use."
        );
    }

    #[test]
    fn a_preview_turns_behind_and_back_only_after_holding() {
        let start = Instant::now();
        let mut keeping = Keeping {
            rate: 9.0,
            ..Keeping::default()
        };
        keeping.judge(30.0, start);
        keeping.judge(30.0, start + ms(2900));
        assert!(!keeping.behind, "not yet");
        keeping.judge(30.0, start + HOLD);
        assert!(keeping.behind);
        // A good second does not clear it, nor does a middling one.
        keeping.rate = 29.0;
        keeping.judge(30.0, start + ms(4000));
        keeping.rate = 25.0;
        keeping.judge(30.0, start + ms(5000));
        keeping.rate = 29.0;
        keeping.judge(30.0, start + ms(6000));
        keeping.judge(30.0, start + ms(8900));
        assert!(keeping.behind);
        keeping.judge(30.0, start + ms(9000));
        assert!(!keeping.behind, "caught up for three seconds");
        // Between the thresholds it stays as it is.
        keeping.rate = 25.0;
        keeping.judge(30.0, start + ms(20_000));
        keeping.judge(30.0, start + ms(30_000));
        assert!(!keeping.behind);
    }

    /// Choose `mode` in Settings, keeping the test's clock.
    fn choose(bench: &mut Workbench, mode: Mode) {
        let now = bench.now;
        let _ = bench.update(Message::Performance(mode));
        bench.now = now;
    }

    fn streaming(bench: &mut Workbench, fps: f64) {
        bench.snapshot.cameras = vec![liveness::streaming_camera(100, 0, fps)];
        bench.snapshot.active_camera = Some("sim:0".into());
        bench.key_window = Some(bench.main_window);
    }

    /// Draw `fps` new frames of `sim:0` a second for `seconds`, ticking
    /// every 250 ms.
    fn draw(bench: &mut Workbench, fps: u64, seconds: u64) {
        for tick in 0..seconds * 4 {
            for _ in fps * tick / 4..fps * (tick + 1) / 4 {
                bench.frame_id = Some(bench.frame_id.unwrap_or_default() + 1);
                bench.draws.drew(bench.now, &bench.frames_shown());
            }
            bench.now += ms(250);
            bench.note_undrawn();
            bench.draws.drew(bench.now, &[]);
            bench.tick_performance();
        }
    }

    #[test]
    fn a_preview_that_falls_behind_shows_its_figure_and_switches_automatic_once() {
        let mut bench = Workbench::new(SessionHandle::new(), "test".into(), true, None);
        streaming(&mut bench, 30.0);
        bench.shown = Some(Shown::Gpu {
            key: MAIN_VIEW.into(),
            size: Size::new(8.0, 8.0),
        });
        draw(&mut bench, 28, 5);
        assert_eq!(bench.preview_figure("sim:0"), None, "keeping up");
        assert!(!efficient());
        draw(&mut bench, 8, 5);
        assert_eq!(
            bench.preview_figure("sim:0").as_deref(),
            Some("Preview 8 of 30 fps")
        );
        assert!(efficient(), "Automatic heard it");
        let notice = bench.notice.clone().unwrap();
        assert!(notice.text.starts_with("Switched to Efficient preview"));
        assert!(notice.detail.unwrap().contains("keep every frame"));
        // Efficient preview draws at most 15 a second, which it manages.
        draw(&mut bench, 15, 5);
        assert_eq!(bench.preview_figure("sim:0"), None);
        // Choosing a setting starts Automatic afresh; the notice is once a run.
        choose(&mut bench, Mode::Smooth);
        choose(&mut bench, Mode::Auto);
        assert!(!efficient());
        bench.notice = None;
        draw(&mut bench, 8, 5);
        assert!(efficient());
        assert!(bench.notice.is_none(), "said once");
    }

    #[test]
    fn previews_are_judged_only_while_the_window_draws_with_the_keyboard() {
        let mut bench = Workbench::new(SessionHandle::new(), "test".into(), true, None);
        streaming(&mut bench, 30.0);
        bench.shown = Some(Shown::Gpu {
            key: MAIN_VIEW.into(),
            size: Size::new(8.0, 8.0),
        });
        bench.key_window = None;
        draw(&mut bench, 4, 6);
        assert_eq!(bench.preview_figure("sim:0"), None, "in the background");
        // A slow camera says too little to judge.
        streaming(&mut bench, 4.0);
        draw(&mut bench, 1, 6);
        assert_eq!(bench.preview_figure("sim:0"), None);
        // Smooth, a slow preview shows its figure but switches nothing.
        choose(&mut bench, Mode::Smooth);
        streaming(&mut bench, 30.0);
        draw(&mut bench, 8, 5);
        assert!(bench.preview_figure("sim:0").is_some());
        assert!(!efficient());
        assert!(bench.preview_about().ends_with("to lower the load."));
    }

    #[test]
    fn without_a_gpu_pipeline_automatic_hears_of_software_rendering() {
        let mut bench = Workbench::new(SessionHandle::new(), "test".into(), true, None);
        process::render_in_software(true);
        for _ in 0..RENDERER_DRAWS - 1 {
            bench.draws.drew(bench.now, &[]);
        }
        bench.tick_performance();
        assert!(!efficient(), "judged only after a few draws");
        bench.draws.drew(bench.now, &[]);
        bench.tick_performance();
        process::render_in_software(false);
        assert!(efficient());
        assert!(bench.refresh.calibrated(), "redraws time the drawing");
        assert_eq!(
            note(
                Mode::Auto,
                bench.performance_state.resolved.as_ref().unwrap()
            )
            .unwrap(),
            "Automatic chose Efficient: software rendering."
        );
    }

    #[test]
    fn the_setting_persists_applies_and_undoes() {
        let prefs = Prefs {
            performance: Mode::Efficient,
            ..Prefs::default()
        };
        let mut bench = Workbench::new(SessionHandle::new(), "test".into(), true, Some(prefs));
        assert!(efficient(), "the saved choice applies at start");
        let _ = bench.update(Message::Performance(Mode::Smooth));
        assert!(!efficient());
        assert_eq!(bench.prefs().performance, Mode::Smooth);
        let _ = bench.update(Message::Undo);
        assert_eq!(bench.performance, Mode::Efficient);
        assert!(efficient(), "undo applies too");
        for open in [true, false] {
            bench.settings_open = open;
            let _ = bench.settings_content(Palette::of(false));
        }
    }
}
