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
/// How long every preview must keep up, while judged, before Automatic
/// hears that the overload is over. It doubles each time that happens in a
/// run, so a computer that keeps falling behind settles on Efficient.
const RELEASE: Duration = Duration::from_secs(120);

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
        /// What the environment variable would set.
        overriding: Option<Mode>,
        hints: Vec<&'static str>,
        forced: Option<bool>,
        software: bool,
    }
    thread_local! { static STATE: RefCell<State> = RefCell::default(); }

    /// A forced answer, else one pinned for code outside the GUI too, such
    /// as `frame::shrink`, else this thread's setting.
    pub(in super::super) fn efficient() -> bool {
        STATE
            .with_borrow(|state| state.forced)
            .or_else(crate::efficiency::pinned)
            .unwrap_or_else(|| current().efficient)
    }
    pub(in super::super) fn current() -> Resolved {
        STATE.with_borrow(|state| {
            let (setting, source) = match state.overriding {
                Some(mode) => (mode, format!("set by {}={mode}", crate::efficiency::ENV)),
                None => (state.preference, "set in the desktop settings".into()),
            };
            match setting {
                Mode::Auto => Resolved {
                    setting,
                    efficient: !state.hints.is_empty(),
                    reasons: state.hints.iter().map(|hint| hint.to_string()).collect(),
                },
                mode => Resolved {
                    setting,
                    efficient: mode == Mode::Efficient,
                    reasons: vec![source],
                },
            }
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
            state.overriding = None;
            state.hints.clear();
        });
    }
    /// Set the environment variable, as far as this thread can tell.
    pub(in super::super) fn override_with(mode: Mode) {
        STATE.with_borrow_mut(|state| state.overriding = Some(mode));
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
    if let Some(overridden) = overridden(chosen, resolved, "this setting") {
        return Some(overridden);
    }
    match chosen {
        Mode::Auto if resolved.efficient => Some(format!(
            "Automatic chose Efficient: {}.",
            resolved.reasons.join(", ")
        )),
        Mode::Auto => Some("Automatic chose Smooth for this computer.".into()),
        _ => None,
    }
}

/// What overrides the `chosen` setting, called `setting`, if the command
/// line or the environment does.
fn overridden(chosen: Mode, resolved: &Resolved, setting: &str) -> Option<String> {
    let mode = if resolved.efficient {
        "Efficient"
    } else {
        "Smooth"
    };
    match resolved.reasons.first() {
        Some(source) if source.starts_with("set by ") => {
            Some(format!("{mode} is {source}, which overrides {setting}."))
        }
        _ if resolved.setting != chosen => Some(format!(
            "The command line or {} overrides {setting}; {mode} is in use.",
            crate::efficiency::ENV
        )),
        _ => None,
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

/// Automatic heard that previews fell behind.
struct Overload {
    /// The cameras whose previews did.
    cameras: Vec<String>,
    /// How long every preview has kept up since, while judged.
    calm: Duration,
}

/// The desktop's performance state, beside the workbench's.
#[derive(Default)]
pub(super) struct PerformanceState {
    /// The preference last handed to the efficiency setting.
    applied: Option<Mode>,
    /// What the setting resolved to at the last tick, for Settings.
    pub(super) resolved: Option<Resolved>,
    renderer_judged: bool,
    overload: Option<Overload>,
    /// How many times Automatic heard an overload was over.
    released: u32,
    /// The notice saying a preview fell behind has shown, once a run.
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
            state.overload = None;
            state.released = 0;
            process::set(self.performance);
        }
        state.applied = Some(self.performance);
        state.resolved = Some(process::current());
    }

    /// The performance bookkeeping on the slow tick: judge the renderer,
    /// count what each preview drew and tell Automatic when it fell behind.
    pub(super) fn tick_performance(&mut self) {
        self.judge_renderer();
        let judged = self.count_presented();
        self.weigh_overload(judged);
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
    /// and has the keyboard, since a background window may draw less, and
    /// knows the display's rate. Returns how long a window just judged.
    fn count_presented(&mut self) -> Option<Duration> {
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
            return None;
        }
        state.since = Some(now);
        let judging = self.screenshot.is_none()
            && !self.frames_paused()
            && self.key_window == Some(self.main_window)
            && self.refresh.calibrated();
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
        judging.then_some(elapsed)
    }

    /// Tell Automatic when a preview falls behind, saying so the first time
    /// that makes the preview efficient. The overload is over once the
    /// cameras behind are no longer shown streaming, or every preview has
    /// kept up for the release time over the windows `judged`.
    fn weigh_overload(&mut self, judged: Option<Duration>) {
        let state = &mut self.performance_state;
        let behind: Vec<String> = state
            .cameras
            .iter()
            .filter(|(_, keeping)| keeping.behind)
            .map(|(camera, _)| camera.clone())
            .collect();
        if let Some(overload) = &mut state.overload {
            if !behind.is_empty() {
                overload.calm = Duration::ZERO;
            } else if let Some(window) = judged {
                overload.calm += window;
            }
            let shown = overload
                .cameras
                .iter()
                .any(|camera| state.cameras.contains_key(camera));
            let hold = RELEASE.saturating_mul(1 << state.released.min(8));
            if !shown || overload.calm >= hold {
                state.overload = None;
                state.released += 1;
                process::clear_hint(OVERLOAD);
            }
            return;
        }
        let auto = state
            .resolved
            .as_ref()
            .is_some_and(|resolved| resolved.setting == Mode::Auto);
        if !auto || behind.is_empty() {
            return;
        }
        state.overload = Some(Overload {
            cameras: behind,
            calm: Duration::ZERO,
        });
        let before = efficient();
        process::hint(OVERLOAD);
        if !before && efficient() && !std::mem::replace(&mut state.noticed, true) {
            self.set_notice(
                "Switched to Efficient mode because this computer was falling behind",
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
        if efficient() {
            about.push_str(" Efficient mode is on.");
        } else if let Some(overridden) = self.performance_overridden() {
            about.push(' ');
            about.push_str(&overridden);
        } else {
            about.push_str(" Use Efficient mode to lower the load.");
        }
        about
    }

    /// What overrides Settings › Performance, if anything does.
    fn performance_overridden(&self) -> Option<String> {
        let resolved = self.performance_state.resolved.as_ref()?;
        overridden(self.performance, resolved, "Settings › Performance")
    }

    /// The neutral chip with the preview figure, whose press turns on
    /// Efficient mode unless it is on or the command line or environment
    /// decides; `shown` fades it with the controls. Its explanation, a
    /// tooltip, is also its accessible description.
    pub(super) fn preview_chip(&self, id: &str, shown: f32) -> Option<Element<'_, Message>> {
        let figure = self.preview_figure(id)?;
        let stage = style::stage_palette(self.key_window == Some(self.main_window));
        let switch = !efficient() && self.performance_overridden().is_none();
        let label = text(figure.clone())
            .size(style::SMALL)
            .color(fade(stage.secondary, shown))
            .wrapping(text::Wrapping::None);
        let chip = button(label)
            .name(if switch {
                format!("{figure}. Use Efficient mode")
            } else {
                figure
            })
            .description(self.preview_about())
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
                "Efficient draws up to 15 camera frames a second, 5 in the overview, keeps scaled-down pictures to 1280 pixels and skips animations. Auto capture checks fewer frames on the CPU, and new recordings encode faster. Captures and recordings keep every frame."
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
        bench.refresh.report(Some(Duration::from_micros(16_667)));
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
        assert!(notice.text.starts_with("Switched to Efficient mode"));
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
    fn automatic_hears_the_overload_is_over_once_previews_keep_up_long_enough() {
        let mut bench = Workbench::new(SessionHandle::new(), "test".into(), true, None);
        streaming(&mut bench, 30.0);
        bench.shown = Some(Shown::Gpu {
            key: MAIN_VIEW.into(),
            size: Size::new(8.0, 8.0),
        });
        draw(&mut bench, 8, 5);
        assert!(efficient());
        // Efficient previews keep up; only time judged counts towards two
        // minutes of it.
        draw(&mut bench, 15, 50);
        bench.key_window = None;
        draw(&mut bench, 15, 120);
        bench.key_window = Some(bench.main_window);
        draw(&mut bench, 15, 50);
        assert!(efficient());
        // Past them this computer keeps up with the camera in Smooth too.
        draw(&mut bench, 28, 30);
        assert!(!efficient(), "kept up long enough");
        assert_eq!(
            note(
                Mode::Auto,
                bench.performance_state.resolved.as_ref().unwrap()
            )
            .unwrap(),
            "Automatic chose Smooth for this computer."
        );
        // Falling behind again switches back quietly, for twice as long.
        bench.notice = None;
        draw(&mut bench, 8, 5);
        assert!(efficient());
        assert!(bench.notice.is_none(), "said once a run");
        draw(&mut bench, 15, 130);
        assert!(efficient(), "four minutes now");
        draw(&mut bench, 28, 120);
        assert!(!efficient());
        // The camera that fell behind stopping ends the overload at once.
        draw(&mut bench, 8, 5);
        assert!(efficient());
        bench.snapshot.cameras[0].streaming = false;
        bench.now += ms(250);
        bench.tick_performance();
        assert!(!efficient());
    }

    #[test]
    fn the_chip_switches_nothing_the_command_line_or_environment_decides() {
        use iced::advanced::{Layout, layout, renderer::Headless, widget::Tree};
        let renderer = iced::futures::executor::block_on(iced::Renderer::new(
            iced::Font::default(),
            iced::Pixels(16.0),
            Some("tiny-skia"),
        ))
        .expect("software renderer");
        // The chip's accessible name, and whether it can be pressed.
        let chip = |bench: &Workbench| {
            let mut chip = bench.preview_chip("sim:0", 1.0).unwrap();
            let mut tree = Tree::new(chip.as_widget());
            let node = chip.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, Size::new(400.0, 100.0)),
            );
            let (native, _, _) = accessibility::snapshot_widget(
                chip.as_widget_mut(),
                &mut tree,
                Layout::new(&node),
                &renderer,
            );
            let (_, button) = native
                .nodes
                .iter()
                .find(|(_, node)| node.role() == accessibility::Role::Button)
                .unwrap();
            // The tooltip's explanation reaches assistive technology too,
            // pressable or not.
            assert_eq!(button.description(), Some(bench.preview_about().as_str()));
            (button.label().unwrap().to_owned(), !button.is_disabled())
        };
        let mut bench = Workbench::new(SessionHandle::new(), "test".into(), true, None);
        choose(&mut bench, Mode::Smooth);
        streaming(&mut bench, 30.0);
        bench.shown = Some(Shown::Gpu {
            key: MAIN_VIEW.into(),
            size: Size::new(8.0, 8.0),
        });
        draw(&mut bench, 8, 5);
        assert_eq!(
            chip(&bench),
            ("Preview 8 of 30 fps. Use Efficient mode".into(), true)
        );
        process::override_with(Mode::Smooth);
        bench.tick_performance();
        assert_eq!(chip(&bench), ("Preview 8 of 30 fps".into(), false));
        assert!(bench.preview_about().ends_with(
            "Smooth is set by CAPTUREFAB_EFFICIENCY=smooth, which overrides Settings › Performance."
        ));
        // An automatic override of a chosen Smooth decides as well.
        process::override_with(Mode::Auto);
        bench.tick_performance();
        assert!(!chip(&bench).1);
        assert!(bench.preview_about().ends_with(
            "The command line or CAPTUREFAB_EFFICIENCY overrides Settings › Performance; Smooth is in use."
        ));
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
        // Nor while the display's refresh rate, the cap, is being measured.
        bench.key_window = Some(bench.main_window);
        bench.refresh.report(None);
        bench.refresh.recalibrate();
        draw(&mut bench, 4, 6);
        assert_eq!(bench.preview_figure("sim:0"), None, "measuring the display");
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
            bench.draws.drawn();
        }
        // iced repeats a draw's first pass after an update made during it.
        bench.draws.drew(bench.now, &[]);
        bench.tick_performance();
        assert!(!efficient(), "judged only after a few draws");
        bench.draws.drew(bench.now, &[]);
        bench.draws.drawn();
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
