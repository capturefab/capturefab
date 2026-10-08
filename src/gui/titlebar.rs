//! The title bar over the main area: pane toggles, what is shown, its live
//! status and its actions; and the hint that says how to leave image mode.
use super::*;
use crate::{scheduling::CaptureJob, session::LogEntry};
use std::collections::HashSet;
use std::hash::{DefaultHasher, Hash, Hasher};

/// State for the title bar and the toolbar.
pub(super) struct ChromeState {
    /// The status pill going live, from Ready (0) to streaming (1): its tint,
    /// and the width of its live section.
    live: Motion,
    /// The capsule saying how to leave image mode, just after entering it.
    hint: Motion,
    /// When image mode was entered; `None` outside it.
    entered: Option<Instant>,
    /// Errors and warnings in the activity log read while it was open, by
    /// `alert_key`.
    seen: HashSet<u64>,
    /// The worst problem logged since the activity log was last open.
    pub(super) unseen: Option<Level>,
    /// Screenshot scenes: an active capture job, and sample log lines.
    scene_job: bool,
    scene_log: bool,
}

impl Default for ChromeState {
    fn default() -> Self {
        Self {
            live: Motion::new(0.0, motion::SLIDE, motion::SLIDE_OUT, Kind::Fade),
            hint: Motion::new(0.0, motion::CONTROLS_IN, motion::CONTROLS_OUT, Kind::Fade),
            entered: None,
            seen: HashSet::new(),
            unseen: None,
            scene_job: false,
            scene_log: false,
        }
    }
}

impl ChromeState {
    super::motion::registry! {
        motions: [live, hint],
        flashes: [],
    }
}

/// The title bar's and toolbar's hooks into the shared update cycle.
impl Workbench {
    /// Point the title bar's motions at what they show; from
    /// `sync_animations`.
    pub(super) fn sync_chrome(&mut self) {
        let now = self.now;
        let live = if self.overview() {
            self.overview_phase().0.live()
        } else {
            self.single_phase().is_some_and(Phase::live)
        };
        self.chrome.live.show(live, now);
        let still = self.screenshot.is_some();
        if still {
            // A still shows where the pill rests, not a frame of its change.
            self.chrome.live.set(if live { 1.0 } else { 0.0 });
        }
        if self.image_mode {
            // A still of image mode shows it at rest; `image-hint` shows entering it.
            if self.chrome.entered.is_none() && !still {
                self.chrome.entered = Some(now);
                // Show the way out at once: the stage controls hold its button.
                self.wake_controls();
                self.controls.show(true, now);
            }
        } else if self.chrome.entered.take().is_some() {
            // The panels are back, which says it all.
            self.chrome.hint.set(0.0);
        }
        let hint = self
            .chrome
            .entered
            .is_some_and(|at| now.saturating_duration_since(at) < motion::CONTROLS_IDLE);
        self.chrome.hint.show(hint, now);
    }

    /// The title bar's and toolbar's bookkeeping on the slow tick, after the
    /// snapshot refresh; from `tick()`.
    pub(super) fn tick_chrome(&mut self) {
        self.patch_chrome_scene();
        let open = self.logs_open && !self.image_mode;
        self.chrome.unseen = unseen(&self.snapshot.logs, &mut self.chrome.seen, open);
    }

    /// Take a screenshot scene word for the title bar or toolbar: `late` is false
    /// while the scene is set up and true once its cameras stream. Returns
    /// whether the word was taken; see `apply_scene`. Words: `scheduled` (the
    /// selected camera has an active capture job), `log-sample` (the
    /// activity log holds a warning, an untimed error and an info line),
    /// `open-camera` (one camera's view while several are connected),
    /// `image-hint` (image mode just entered, with its hint and controls).
    pub(super) fn scene_chrome(&mut self, word: &str, late: bool) -> bool {
        match (word, late) {
            ("scheduled", _) => self.chrome.scene_job = true,
            ("log-sample", _) => self.chrome.scene_log = true,
            ("open-camera", _) => self.focus_camera = true,
            ("image-hint", false) => {
                // Entered at once, so the panels have folded by the shot;
                // left untaken, so it comes back once the cameras stream.
                self.image_mode = true;
                return false;
            }
            ("image-hint", true) => {
                self.chrome.entered = Some(self.now);
                self.chrome.hint.set(1.0);
                self.wake_controls();
                self.controls.set(1.0);
            }
            _ => return false,
        }
        true
    }

    /// Make the snapshot hold what the chrome scene words add; after every
    /// refresh, so it never reverts.
    fn patch_chrome_scene(&mut self) {
        let snapshot = &mut self.snapshot;
        if self.chrome.scene_job && !snapshot.jobs.iter().any(CaptureJob::active) {
            let now = crate::scheduling::now_ms();
            snapshot.jobs.push(CaptureJob {
                id: 3,
                status: "pending".into(),
                output: "captures".into(),
                count: 120,
                captured: 18,
                first_at_ms: now.saturating_sub(18 * 60_000),
                interval_ms: 60_000,
                next_at_ms: now + 42_000,
                last_file: None,
                error: None,
                destination: None,
            });
        }
        if self.chrome.scene_log && !snapshot.logs.iter().any(|entry| entry.level == "warn") {
            for (time, level, message) in SCENE_LOG {
                snapshot.logs.push(LogEntry {
                    time: time.into(),
                    level: level.into(),
                    message: message.into(),
                });
            }
        }
    }
}

/// Log lines for the `log-sample` scene: a warning, a command failure,
/// which the session logs without a time, and news.
const SCENE_LOG: [(&str, &str, &str); 3] = [
    (
        "04:12:07",
        "warn",
        "USB discovery: access denied; check the device permissions",
    ),
    ("", "error", "the camera stopped responding"),
    ("04:12:31", "info", "Saved capture-0002.png"),
];

/// Width of the fps figure's slot in a status pill.
const FIGURE: f32 = 52.0;
/// Width of a status pill's live section at rest: the sparkline, a gap and
/// the figure's slot.
const LIVE: f32 = 64.0 + 6.0 + FIGURE;

/// A rounded status label: a steady dot, a word, and an optional live
/// section of a `chart` and a `figure`. With a `slot`, the figure keeps that
/// fixed width, set against its right edge, so a changing figure never
/// shifts what follows. `reveal` shows the live section, from 0 (hidden) to
/// 1 (all of it), for animating the change into and out of streaming.
pub(super) fn status_pill<'a>(
    label: &'a str,
    figure: Option<String>,
    slot: Option<f32>,
    chart: Option<Element<'a, Message>>,
    color: Color,
    reveal: f32,
) -> Element<'a, Message> {
    let mut content = row![
        dot(color, 6.0),
        space().width(6),
        text(label)
            .size(style::SMALL)
            .font(style::MEDIUM)
            .wrapping(text::Wrapping::None),
    ]
    .align_y(Alignment::Center);
    // The gap before the live section folds away with it.
    let mut live = row![space().width(6)].align_y(Alignment::Center);
    let empty = chart.is_none() && figure.is_none();
    if let Some(chart) = chart {
        live = live.push(chart).push(space().width(6));
    }
    if let Some(figure) = figure {
        let figure = text(figure)
            .size(style::SMALL)
            .wrapping(text::Wrapping::None);
        live = live.push(match slot {
            Some(width) => figure.width(width).align_x(Alignment::End),
            None => figure,
        });
    }
    if !empty && reveal > 0.001 {
        content = content.push(
            container(live)
                .max_width((6.0 + LIVE) * reveal.min(1.0))
                .clip(true),
        );
    }
    container(content)
        .padding([3, 10])
        .style(style::pill(color))
        .into()
}

/// A camera's recent frame rate, with lost frames marked; a faint line
/// until there are enough readings for a trend, so the slot never looks
/// broken.
pub(super) fn fps_spark<'a>(
    history: Option<&'a sparkline::History>,
    color: Color,
    flag: Color,
    size: (f32, f32),
) -> Element<'a, Message> {
    if !history.is_some_and(sparkline::History::ready) {
        let (width, height) = size;
        return tip(
            container(rule::horizontal(1).style(move |_| rule::Style {
                color: fade(color, 0.3),
                radius: iced::border::Radius::default(),
                fill_mode: rule::FillMode::Full,
                snap: true,
            }))
            .width(width)
            .center_y(height),
            "Collecting frame rate…",
        );
    }
    spark(history, color, flag, size, |history| {
        trend(
            history,
            "Frame rate",
            |fps| format!("{fps:.1} fps"),
            "lost frames",
        )
    })
}

/// What the status pill says.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Phase {
    Ready,
    Starting,
    Stopping,
    Streaming,
    /// Streaming, but no new frame for this long; see `Workbench::stalled`.
    Waiting(Duration),
}

impl Phase {
    fn label(self) -> &'static str {
        match self {
            Phase::Ready => "Ready",
            Phase::Starting => "Starting…",
            Phase::Stopping => "Stopping…",
            Phase::Streaming => "Streaming",
            Phase::Waiting(_) => "Waiting for frames",
        }
    }

    /// The camera state whose dot color the pill takes once live, so it
    /// agrees with the camera's dot elsewhere.
    fn state(self) -> CameraState {
        match self {
            Phase::Streaming => CameraState::Live,
            Phase::Waiting(_) => CameraState::Stalled,
            Phase::Ready | Phase::Starting | Phase::Stopping => CameraState::Idle,
        }
    }

    /// Whether the pill shows its live section, tinted as live.
    fn live(self) -> bool {
        matches!(self, Phase::Streaming | Phase::Waiting(_))
    }

    /// While a start or stop is on its way, whether it is a stop.
    fn pending(self) -> Option<bool> {
        match self {
            Phase::Starting => Some(false),
            Phase::Stopping => Some(true),
            _ => None,
        }
    }
}

/// The overview's Auto label for `on` of `total` cameras in auto mode, and
/// whether it reads as on: all of them.
fn auto_all(on: usize, total: usize) -> (String, bool) {
    if on == 0 || on == total {
        ("Auto all".into(), on == total && total > 0)
    } else {
        (format!("Auto {on} of {total}"), false)
    }
}

/// The color of the dot in a running Auto button: settled, limited, or
/// still at work.
fn auto_color(state: &str, p: &Palette) -> Color {
    match state {
        "stable" => p.live,
        "limited" => p.warn,
        _ => p.accent,
    }
}

/// What auto mode is doing, for the Auto button's tooltip, with its notes.
fn auto_about(status: &AutoStatus, os: Os) -> String {
    let state = match status.state.as_str() {
        "stable" => "Auto has settled",
        "limited" => "Auto is at its limits",
        "converging" => "Auto is adjusting exposure",
        "waiting" => "Auto is waiting for frames",
        _ => "Auto is on",
    };
    let mut about = Action::ToggleAuto.hint(state, os);
    for note in &status.notes {
        about.push('\n');
        about.push_str(&capitalize(note));
    }
    about.push_str("\nClick for manual");
    about
}

/// Tell a stable hash of a log line, to know it again after the log scrolls.
fn alert_key(entry: &LogEntry) -> u64 {
    let mut hasher = DefaultHasher::new();
    (&entry.time, &entry.level, &entry.message).hash(&mut hasher);
    hasher.finish()
}

/// Forget read problems beyond this many, keeping those still in the log.
const SEEN_MAX: usize = 512;

/// The worst problem in `logs` not read yet: errors and warnings that came
/// while the log was closed. With the log `open`, everything in it counts as
/// read. Lines are known again by content, so the log may scroll, or switch
/// to another camera's, without a problem counting twice.
fn unseen(logs: &[LogEntry], seen: &mut HashSet<u64>, open: bool) -> Option<Level> {
    let mut worst = None;
    for entry in logs {
        let Some(level) = Level::from_log(&entry.level) else {
            continue;
        };
        let key = alert_key(entry);
        if open {
            seen.insert(key);
        } else if !seen.contains(&key) && worst != Some(Level::Error) {
            worst = Some(level);
        }
    }
    if seen.len() > SEEN_MAX {
        let present: HashSet<u64> = logs.iter().map(alert_key).collect();
        seen.retain(|key| present.contains(key));
    }
    worst
}

/// A button styled as a status pill tinted with `color`, a little deeper
/// while hovered or pressed.
fn tag_style(color: Color) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |theme, status| {
        let pill = style::pill(color)(theme);
        let tint = match status {
            button::Status::Hovered => 0.2,
            button::Status::Pressed => 0.22,
            _ => 0.14,
        };
        button::Style {
            background: Some(Color { a: tint, ..color }.into()),
            text_color: pill.text_color.unwrap_or(color),
            border: pill.border,
            ..button::Style::default()
        }
    }
}

/// Rough width of `text` set at `size`, for deciding what fits.
fn rough(text: &str, size: f32) -> f32 {
    text.chars().count() as f32 * size * 0.5
}

/// A clickable status pill: `mark` and any `label`, sending `on`,
/// explained by `about`.
fn tag<'a>(
    mark: Element<'a, Message>,
    label: Option<String>,
    color: Color,
    on: Message,
    about: String,
) -> Element<'a, Message> {
    let padding = if label.is_some() { [3, 10] } else { [3, 8] };
    let mut content = row![mark].spacing(6).align_y(Alignment::Center);
    if let Some(label) = label {
        content = content.push(
            text(label)
                .size(style::SMALL)
                .font(style::MEDIUM)
                .wrapping(text::Wrapping::None),
        );
    }
    tip(
        button(content)
            .padding(padding)
            .style(tag_style(color))
            .on_press(on),
        about,
    )
}

impl Workbench {
    /// What the single-camera status pill says; `None` with no camera.
    fn single_phase(&self) -> Option<Phase> {
        let snapshot = &self.snapshot;
        snapshot.connected.as_ref()?;
        let id = snapshot.active_camera.as_deref();
        Some(match id.and_then(|id| self.stream_pending(id)) {
            Some(false) => Phase::Starting,
            Some(true) => Phase::Stopping,
            None if snapshot.streaming => id
                .and_then(|id| self.stalled(id))
                .map_or(Phase::Streaming, Phase::Waiting),
            None => Phase::Ready,
        })
    }

    /// What the overview's status pill says, with each streaming camera
    /// that waits for frames and how long it has waited.
    fn overview_phase(&self) -> (Phase, Vec<(&CameraInfo, Duration)>) {
        let streaming: Vec<_> = self
            .snapshot
            .cameras
            .iter()
            .filter(|camera| camera.streaming)
            .collect();
        let waiting: Vec<_> = streaming
            .iter()
            .filter_map(|camera| Some((&camera.info, self.stalled(&camera.info.id)?)))
            .collect();
        // Start all and Stop all; a single camera's tile shows its own.
        let batch = |job: Job| self.pending.iter().any(|p| p.batch && p.job == job);
        let phase = if batch(Job::Start) {
            Phase::Starting
        } else if batch(Job::Stop) {
            Phase::Stopping
        } else if let Some(longest) = waiting.iter().map(|(_, silent)| *silent).max() {
            Phase::Waiting(longest)
        } else if streaming.is_empty() {
            Phase::Ready
        } else {
            Phase::Streaming
        };
        (phase, waiting)
    }

    /// How far the status pill has gone live: its tint, and the width of
    /// its live section, which snaps under Reduce Motion.
    fn live_level(&self) -> (f32, f32) {
        let tint = self.chrome.live.get(self.now);
        let width = if motion::reduce_motion() {
            self.chrome.live.target()
        } else {
            tint
        };
        (tint, width)
    }

    /// What tells the camera shown apart, when another connected camera is
    /// the same model, so the title alone would not; see `twin_identity`.
    fn twin_identity(&self) -> Option<String> {
        let camera = self.snapshot.connected.as_ref()?;
        twin_identity(&self.snapshot.cameras, camera)
    }

    /// The single-camera title bar: the camera's model, its live status,
    /// what it records, forwards or has scheduled, and Auto, Start/Stop and
    /// the way back to all cameras.
    pub(super) fn single_bar(&self, p: &'static Palette) -> Element<'_, Message> {
        let snapshot = &self.snapshot;
        let os = Os::CURRENT;
        let phase = self.single_phase();
        let title = snapshot
            .connected
            .as_ref()
            .map_or(String::new(), |camera| camera.model.clone());
        let jobs: Vec<_> = snapshot.jobs.iter().filter(|job| job.active()).collect();
        let tags = usize::from(snapshot.forwarding.is_some()) + usize::from(!jobs.is_empty());
        let (labels, chart) = self.bar_fit(&title, phase.map_or("", Phase::label), tags);
        let mut status = row![].spacing(6).align_y(Alignment::Center);
        if let Some(phase) = phase {
            let (tint, reveal) = self.live_level();
            let on = phase.state().color(p);
            // Drawn while it folds away too.
            let live = reveal > 0.001;
            let history = snapshot
                .active_camera
                .as_ref()
                .and_then(|id| self.throughput.get(id));
            let pill = status_pill(
                phase.label(),
                live.then(|| format!("{:.1} fps", snapshot.fps)),
                chart.then_some(FIGURE),
                (live && chart).then(|| fps_spark(history, on, p.warn, (64.0, 14.0))),
                style::mix(p.accent_text, on, tint),
                reveal,
            );
            status = status.push(match phase {
                Phase::Waiting(silent) => tip(pill, silence(silent)),
                _ => pill,
            });
        }
        if let Some(target) = &snapshot.forwarding {
            status = status.push(output_tag(target, labels, p));
        }
        if !jobs.is_empty() {
            status = status.push(jobs_tag(&jobs, labels, p));
        }
        let mut actions = row![].spacing(8).align_y(Alignment::Center);
        if snapshot.cameras.len() > 1 {
            actions = actions.push(tip(
                button(
                    row![
                        icon(Icon::Grid, 14.0, p.text),
                        text("All cameras").size(style::BODY)
                    ]
                    .spacing(7)
                    .align_y(Alignment::Center),
                )
                .padding([5, 12])
                .style(style::secondary)
                .on_press(Message::Focus(false)),
                Action::Overview.hint("Back to all cameras", os),
            ));
        }
        if let Some(phase) = phase {
            let stopping = phase.pending();
            // While a start or stop is on its way, the button keeps the
            // label it was pressed with.
            let streaming = stopping.unwrap_or(snapshot.streaming);
            actions = actions.push(self.auto_button(p)).push(tip(
                stream_button(
                    streaming,
                    if streaming { "Stop" } else { "Start" },
                    Some(Message::ToggleStream),
                    stopping.is_some(),
                    self.spin(),
                    p,
                ),
                match stopping {
                    Some(true) => "Stopping the stream…".into(),
                    Some(false) => "Starting the stream…".into(),
                    None => Action::ToggleStream.hint("Start or stop acquisition", os),
                },
            ));
        }
        self.title_bar(title, Some(status.into()), actions.into(), p)
    }

    /// Width of the main area between the panes, as laid out now.
    pub(super) fn main_width(&self) -> f32 {
        let mut main = self.width - self.sidebar_slide.lerp(0.0, SIDEBAR, self.now);
        if self.inspector_docked() {
            main -= self.inspector_slide.lerp(0.0, INSPECTOR, self.now) + 1.0;
        }
        main
    }

    /// What fits beside the single-camera title, from a rough budget of
    /// the bar's width: whether `tags` (recording, scheduled) keep their
    /// labels, then whether the status pill keeps its sparkline. Labels
    /// go first, as the tags' glyphs and tooltips still say what they are.
    fn bar_fit(&self, title: &str, label: &str, tags: usize) -> (bool, bool) {
        let snapshot = &self.snapshot;
        let sidebar = self.sidebar_slide.lerp(0.0, SIDEBAR, self.now);
        let padding = 20.0 + (self.lights() - sidebar).max(0.0);
        // Auto, Start/Stop and the inspector toggle, with their gaps.
        let mut actions = 64.0 + 76.0 + 28.0 + 3.0 * 8.0;
        if snapshot.auto.is_some() || self.auto_busy() {
            actions += 18.0;
        }
        if self.shown.is_some() {
            actions += 28.0 + 8.0;
        }
        if snapshot.cameras.len() > 1 {
            actions += 126.0 + 8.0;
        }
        // The sidebar toggle, the title and the gaps around it.
        let mut room = self.main_width() - padding - actions - 26.0 - 30.0;
        room -= rough(title, style::TITLE);
        if let Some(identity) = self.twin_identity() {
            room -= 10.0 + rough(&identity, style::BODY);
        }
        let pill = rough(label, style::SMALL) + 32.0 + 6.0 + LIVE;
        let tag = rough("Forwarding", style::SMALL) + 44.0;
        let labels = room >= pill + tags as f32 * tag;
        let tags = if labels { tag } else { 36.0 } * tags as f32;
        (labels, room >= pill + tags)
    }

    /// Auto for the camera shown: tinted while on, with a dot for how auto
    /// is doing, and the spinner while it switches.
    fn auto_button(&self, p: &'static Palette) -> Element<'_, Message> {
        let os = Os::CURRENT;
        let status = self.snapshot.auto.as_ref();
        let on = status.is_some();
        let busy = self.auto_busy();
        let mut label = row![].spacing(6).align_y(Alignment::Center);
        if busy {
            let ink = if on { p.ink(p.accent) } else { p.secondary };
            label = label.push(icon::spinner(12.0, ink, self.spin()));
        } else if let Some(status) = status {
            label = label.push(dot(auto_color(&status.state, p), 6.0));
        }
        label = label.push(text("Auto").size(style::BODY).font(style::MEDIUM));
        let about = if busy {
            "Switching auto mode…".to_owned()
        } else if let Some(status) = status {
            auto_about(status, os)
        } else {
            Action::ToggleAuto.hint("Tune exposure, gain and frame rate", os)
        };
        tip(
            button(label)
                .padding([5, 12])
                // Ignoring presses would otherwise read as disabled.
                .style(move |theme, status| {
                    style::toggle(on)(theme, if busy { button::Status::Active } else { status })
                })
                .on_press_maybe((!busy).then_some(Message::Auto(!on))),
            about,
        )
    }

    /// The overview's title bar: how many cameras stream, and actions for
    /// all of them.
    pub(super) fn overview_bar(&self, p: &'static Palette) -> Element<'_, Message> {
        let snapshot = &self.snapshot;
        let total = snapshot.cameras.len();
        let streaming = snapshot.cameras.iter().filter(|c| c.streaming).count();
        let (phase, waiting) = self.overview_phase();
        let (tint, reveal) = self.live_level();
        let on = phase.state().color(p);
        let pill = status_pill(
            phase.label(),
            (reveal > 0.001).then(|| match phase {
                Phase::Waiting(_) => format!("{} of {streaming}", waiting.len()),
                _ => format!("{streaming} of {total}"),
            }),
            None,
            None,
            style::mix(p.accent_text, on, tint),
            reveal,
        );
        let status = if waiting.is_empty() {
            pill
        } else {
            tip(
                pill,
                waiting
                    .iter()
                    .map(|(camera, silent)| {
                        format!(
                            "{} · {}: {}",
                            camera.model,
                            identity(camera),
                            silence(*silent).to_lowercase()
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
        };
        let auto_on = snapshot.cameras.iter().filter(|c| c.auto.is_some()).count();
        let (auto_label, all_on) = auto_all(auto_on, total);
        let auto_busy = self.auto_busy();
        let mut auto = row![].spacing(6).align_y(Alignment::Center);
        if auto_busy {
            let ink = if all_on { p.ink(p.accent) } else { p.secondary };
            auto = auto.push(icon::spinner(12.0, ink, self.spin()));
        }
        auto = auto.push(text(auto_label).size(style::BODY).font(style::MEDIUM));
        let stopping = phase.pending();
        let stop = stopping.unwrap_or(snapshot.cameras.iter().all(|c| c.streaming));
        let actions = row![
            tip(
                button(auto)
                    .padding([5, 12])
                    .style(move |theme, status| {
                        let status = if auto_busy {
                            button::Status::Active
                        } else {
                            status
                        };
                        style::toggle(all_on)(theme, status)
                    })
                    .on_press_maybe((!auto_busy).then_some(Message::AllAuto(!all_on))),
                if auto_busy {
                    "Switching auto mode…"
                } else if all_on {
                    "Switch every camera to manual"
                } else {
                    "Tune exposure, gain and frame rate on every camera"
                },
            ),
            tip(
                stream_button(
                    stop,
                    if stop { "Stop all" } else { "Start all" },
                    Some(Message::AllStreams(!stop)),
                    stopping.is_some(),
                    self.spin(),
                    p,
                ),
                match stopping {
                    Some(true) => "Stopping the streams…",
                    Some(false) => "Starting the streams…",
                    None if stop => "Stop every camera",
                    None => "Start every camera",
                },
            ),
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        self.title_bar("All cameras".into(), Some(status), actions.into(), p)
    }

    /// The row above the main area: pane toggles, what is shown, its live
    /// status and its actions. It folds away in image mode.
    pub(super) fn title_bar<'a>(
        &self,
        title: String,
        status: Option<Element<'a, Message>>,
        actions: Element<'a, Message>,
        p: &'static Palette,
    ) -> Element<'a, Message> {
        let os = Os::CURRENT;
        let sidebar = self.sidebar_slide.lerp(0.0, SIDEBAR, self.now);
        let mut left = row![tip(
            icon_button(
                Icon::Sidebar,
                16.0,
                if self.sidebar_open {
                    p.text
                } else {
                    p.secondary
                },
                Message::ToggleSidebar,
            ),
            Action::ToggleSidebar.hint("Camera list", os),
        )]
        .spacing(10)
        .align_y(Alignment::Center);
        if !title.is_empty() {
            left = left.push(clipped(
                text(title)
                    .size(style::TITLE)
                    .font(style::BOLD)
                    .wrapping(text::Wrapping::None),
            ));
            if let Some(identity) = (!self.overview()).then(|| self.twin_identity()).flatten() {
                left = left.push(clipped(
                    text(identity)
                        .size(style::BODY)
                        .color(p.secondary)
                        .wrapping(text::Wrapping::None),
                ));
            }
        }
        if let Some(status) = status {
            left = left.push(status);
        }
        let mut bar = row![container(left).width(Fill).clip(true), actions]
            .spacing(8)
            .align_y(Alignment::Center);
        // Image mode needs an image; without one it would only hide the panes.
        if self.shown.is_some() || self.overview() {
            bar = bar.push(tip(
                icon_button(Icon::CornersOut, 16.0, p.secondary, Message::ToggleImage),
                Action::ImageMode.hint("Image only", os),
            ));
        }
        let bar = bar.push(tip(
            icon_button(
                Icon::Sliders,
                16.0,
                if self.inspector_shown() {
                    p.text
                } else {
                    p.secondary
                },
                Message::ToggleInspector,
            ),
            Action::ToggleInspector.hint("Camera settings", os),
        ));
        let bar = container(bar)
            .height(BAR)
            .width(Fill)
            .align_y(Alignment::Center)
            .padding(iced::Padding {
                top: 0.0,
                right: 10.0,
                bottom: 0.0,
                left: 10.0 + (self.lights() - sidebar).max(0.0),
            });
        let bar: Element<'a, Message> = if cfg!(target_os = "macos") {
            mouse_area(bar)
                .on_press(Message::DragWindow)
                .on_double_click(Message::ZoomWindow)
                .into()
        } else {
            bar.into()
        };
        container(bar)
            .height(self.bars.lerp(0.0, BAR, self.now))
            .clip(true)
            .into()
    }

    /// Over the main area just after entering image mode: how to show the
    /// panels again, fading with the stage controls that also appear then.
    /// Takes no input, so the stage under it keeps the pointer.
    pub(super) fn image_hint(&self) -> Element<'_, Message> {
        let shown = self.chrome.hint.get(self.now);
        if shown <= 0.01 {
            return space().into();
        }
        let os = Os::CURRENT;
        container(
            container(
                text(format!(
                    "Press {} or {} to show the panels",
                    Action::ImageMode.key_label(os),
                    Action::Overview.key_label(os)
                ))
                .size(style::SMALL)
                .color(fade(style::ON_STAGE, shown)),
            )
            .padding([5, 12])
            .style(style::overlay(shown)),
        )
        .center_x(Fill)
        .padding(iced::Padding {
            top: 16.0 - motion::rise(6.0, shown),
            ..iced::Padding::ZERO
        })
        .into()
    }
}

/// What the camera shown records to (a local file, in red) or forwards to
/// (a URL, with its credentials hidden); opens the Forward tab.
fn output_tag<'a>(target: &str, labelled: bool, p: &'static Palette) -> Element<'a, Message> {
    let output = Output::of(target);
    let (mark, color): (Element<'a, Message>, _) = if output.recording() {
        (dot(p.danger, 6.0), p.danger)
    } else {
        (icon(Icon::Broadcast, 12.0, p.accent_text), p.accent_text)
    };
    tag(
        mark,
        labelled.then(|| output.title().to_owned()),
        color,
        Message::ShowTab(Tab::Forward),
        format!("{}\nClick for Forward settings", output.about()),
    )
}

/// The camera's active capture jobs; opens the Capture tab.
fn jobs_tag<'a>(jobs: &[&CaptureJob], labelled: bool, p: &'static Palette) -> Element<'a, Message> {
    let about = match jobs {
        [job] => format!(
            "Capture job {} to {} · {} taken",
            job.id,
            job.output,
            grouped(job.captured.into())
        ),
        _ => format!("{} capture jobs scheduled", jobs.len()),
    };
    tag(
        icon(Icon::Recent, 12.0, p.accent_text),
        labelled.then(|| match jobs.len() {
            1 => "Scheduled".to_owned(),
            n => format!("{n} scheduled"),
        }),
        p.accent_text,
        Message::ShowTab(Tab::Capture),
        format!("{about}\nClick for Capture settings"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use liveness::streaming_camera;

    fn bench() -> Workbench {
        let mut bench = Workbench::new(SessionHandle::new(), "test".into(), true, None);
        let camera = streaming_camera(10, 0, 30.0);
        bench.snapshot.connected = Some(camera.info.clone());
        bench.snapshot.active_camera = Some(camera.info.id.clone());
        bench.snapshot.cameras = vec![camera];
        bench
    }

    fn log(time: &str, level: &str, message: &str) -> LogEntry {
        LogEntry {
            time: time.into(),
            level: level.into(),
            message: message.into(),
        }
    }

    #[test]
    fn pill_states_what_the_stream_does() {
        let mut bench = bench();
        assert_eq!(bench.single_phase(), Some(Phase::Ready));
        bench.snapshot.streaming = true;
        assert_eq!(bench.single_phase(), Some(Phase::Streaming));
        bench.send_to("sim:0", Job::Stop, SessionCommand::Stop);
        assert_eq!(bench.single_phase(), Some(Phase::Stopping));
        assert_eq!(Phase::Stopping.pending(), Some(true));
        assert!(!Phase::Stopping.live(), "folds as soon as Stop is pressed");
        // Start all and Stop all reach the camera shown too.
        bench.pending.clear();
        bench.snapshot.streaming = false;
        bench.send_batch(vec!["sim:0".into()], Job::Start, || SessionCommand::Start);
        assert_eq!(bench.single_phase(), Some(Phase::Starting));
        bench.snapshot.connected = None;
        assert_eq!(bench.single_phase(), None);
        assert_eq!(silence(Duration::from_millis(6400)), "No new frame for 6 s");
        assert_eq!(Phase::Waiting(Duration::ZERO).label(), "Waiting for frames");
    }

    #[test]
    fn overview_auto_reads_as_on_only_when_all_are() {
        assert_eq!(auto_all(0, 4), ("Auto all".into(), false));
        assert_eq!(auto_all(2, 4), ("Auto 2 of 4".into(), false));
        assert_eq!(auto_all(4, 4), ("Auto all".into(), true));
        assert_eq!(auto_all(0, 0), ("Auto all".into(), false));
    }

    #[test]
    fn going_live_reveals_the_pill_and_settles() {
        let mut bench = bench();
        let start = bench.now;
        bench.snapshot.streaming = true;
        bench.sync_animations();
        assert_eq!(bench.chrome.live.target(), 1.0);
        bench.now = start + motion::SLIDE / 2;
        let (tint, _) = bench.live_level();
        assert!(tint > 0.0 && tint < 1.0);
        assert!(bench.animating());
        bench.now = start + motion::SLIDE;
        assert_eq!(bench.live_level(), (1.0, 1.0));
        assert!(!bench.chrome.live.animating(bench.now));
        let stop = bench.now;
        bench.snapshot.streaming = false;
        bench.sync_animations();
        assert_eq!(bench.chrome.live.target(), 0.0);
        bench.now = stop + motion::SLIDE_OUT;
        assert_eq!(bench.live_level(), (0.0, 0.0));
        assert!(!bench.chrome.motions().any(|m| m.animating(bench.now)));
    }

    #[test]
    fn image_mode_shows_the_way_out_then_settles() {
        let mut bench = bench();
        let start = bench.now;
        bench.image_mode = true;
        bench.sync_animations();
        assert_eq!(bench.chrome.hint.target(), 1.0);
        assert_eq!(bench.controls.target(), 1.0, "controls wake with it");
        bench.now = start + motion::CONTROLS_IDLE;
        bench.sync_animations();
        assert_eq!(
            bench.chrome.hint.target(),
            0.0,
            "leaves after the idle time"
        );
        bench.now += motion::CONTROLS_OUT;
        assert!(!bench.chrome.hint.animating(bench.now));
        // Shown again on the next entry, and gone at once on leaving.
        bench.image_mode = false;
        bench.sync_animations();
        bench.image_mode = true;
        bench.sync_animations();
        bench.now += motion::CONTROLS_IN;
        assert_eq!(bench.chrome.hint.get(bench.now), 1.0);
        bench.image_mode = false;
        bench.sync_animations();
        assert_eq!(bench.chrome.hint.get(bench.now), 0.0);
        assert!(!bench.chrome.hint.animating(bench.now));
    }

    #[test]
    fn problems_count_until_the_log_is_read() {
        let mut seen = HashSet::new();
        let mut logs = vec![log("10:00:00", "info", "Connected to sim:0")];
        assert_eq!(unseen(&logs, &mut seen, false), None);
        logs.push(log("10:00:01", "warn", "USB discovery: access denied"));
        assert_eq!(unseen(&logs, &mut seen, false), Some(Level::Warning));
        logs.push(log("", "error", "the camera stopped responding"));
        logs.push(log("10:00:03", "warn", "slow link"));
        assert_eq!(
            unseen(&logs, &mut seen, false),
            Some(Level::Error),
            "the worst wins"
        );
        assert_eq!(unseen(&logs, &mut seen, true), None, "read while open");
        assert_eq!(unseen(&logs, &mut seen, false), None);
        // The log scrolls past old lines, or shows another camera's.
        logs.remove(0);
        logs.insert(0, log("09:59:59", "WARN", "older"));
        assert_eq!(unseen(&logs, &mut seen, false), Some(Level::Warning));
    }

    #[test]
    fn crowded_bars_drop_tag_labels_then_the_sparkline() {
        let mut bench = bench();
        bench.sidebar_slide.set(1.0);
        bench.inspector_slide.set(1.0);
        let fit = |bench: &Workbench| bench.bar_fit("Pattern camera", "Streaming", 2);
        bench.width = 1440.0;
        assert_eq!(fit(&bench), (true, true));
        // Narrow: the inspector floats, so the main area is wider again.
        bench.width = 960.0;
        assert_eq!(fit(&bench), (false, true));
        bench.width = motion::NARROW;
        assert_eq!(fit(&bench), (false, false));
        bench.sidebar_slide.set(0.0);
        assert!(bench.bar_fit("Pattern camera", "Streaming", 0).1);
    }

    #[test]
    fn twins_say_which_one_is_shown() {
        let mut bench = bench();
        assert_eq!(bench.twin_identity(), None);
        let mut twin = streaming_camera(10, 0, 30.0);
        twin.info.id = "sim:1".into();
        twin.info.serial = "SIM1".into();
        bench.snapshot.cameras.push(twin);
        let serial = bench.snapshot.connected.as_ref().unwrap().serial.clone();
        assert_eq!(bench.twin_identity(), Some(format!("S/N {serial}")));
        // Stream inputs go by their address, never their hash serial.
        for camera in &mut bench.snapshot.cameras {
            camera.info.model = "RTSP input".into();
            camera.info.address = Some(format!("rtsp://{}.local/live", camera.info.id));
        }
        bench.snapshot.connected = Some(bench.snapshot.cameras[0].info.clone());
        let id = &bench.snapshot.cameras[0].info.id;
        assert_eq!(bench.twin_identity(), Some(format!("{id}.local/live")));
    }

    #[test]
    fn chrome_scene_words_patch_the_snapshot() {
        let mut bench = bench();
        assert!(bench.scene_chrome("scheduled", false));
        assert!(bench.scene_chrome("log-sample", false));
        assert!(
            !bench.scene_chrome("image-hint", false),
            "waits for pictures"
        );
        assert!(bench.image_mode);
        bench.tick_chrome();
        bench.tick_chrome();
        assert_eq!(bench.snapshot.jobs.iter().filter(|j| j.active()).count(), 1);
        assert_eq!(bench.snapshot.logs.len(), SCENE_LOG.len(), "once");
        assert_eq!(bench.chrome.unseen, Some(Level::Error));
        assert!(bench.scene_chrome("image-hint", true));
        assert_eq!(bench.chrome.hint.get(bench.now), 1.0);
    }
}
