//! The title bar over the main area: pane toggles, what is shown, its live
//! status and its actions; and the hint that says how to leave image mode.
use super::*;
use crate::{scheduling::CaptureJob, session::LogEntry};
use std::collections::{HashMap, HashSet};
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
    /// `alert_keys`.
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
/// which the session logs without a time and long enough to wrap, and news.
const SCENE_LOG: [(&str, &str, &str); 3] = [
    (
        "04:12:07",
        "warn",
        "USB discovery: access denied; check the device permissions",
    ),
    (
        "",
        "error",
        "the camera stopped responding: reading register 0x0938 timed out after 3 attempts: \
         no reply from 192.168.1.20:3956 within 500 ms; check the cable and the camera's power",
    ),
    ("04:12:31", "info", "Saved capture-0002.png"),
];

/// Width of the fps figure's slot in a status pill.
const FIGURE: f32 = 52.0;
/// Width of the frame rate chart in a status pill.
const CHART: f32 = 64.0;

/// A rounded status label: a steady dot, a word, and an optional live
/// section of a `chart` (`CHART` wide) and a `figure`. With a `slot`, the
/// figure keeps that fixed width, set against its right edge, so a changing
/// figure never shifts what follows. `reveal` uncovers the live section
/// from its left edge, from 0 (hidden) to 1 (all of it), for animating the
/// change into and out of streaming.
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
    let full = live_width(chart.is_some(), figure.as_deref(), slot);
    if full > 0.0 && reveal > 0.001 {
        // The gap before the live section folds away with it.
        let mut live = row![space().width(6)].align_y(Alignment::Center);
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
        // Uncovered at its own width, never squeezed: a scrollable lays its
        // content out with all the room it wants, and clips it.
        let live = scrollable(live).direction(scrollable::Direction::Horizontal(
            scrollable::Scrollbar::hidden(),
        ));
        content = content.push(container(live).max_width(reveal_cap(full, reveal)));
    }
    container(content)
        .padding([3, 10])
        .style(style::pill(color))
        .into()
}

/// Width of a status pill's live section: the gap before it, any chart and
/// the gap after it, and a `figure` in its `slot`, or as wide as it roughly
/// is. Zero when it holds nothing.
fn live_width(chart: bool, figure: Option<&str>, slot: Option<f32>) -> f32 {
    let figure = figure.map(|figure| slot.unwrap_or_else(|| rough(figure, style::SMALL)));
    if !chart && figure.is_none() {
        return 0.0;
    }
    6.0 + if chart { CHART + 6.0 } else { 0.0 } + figure.unwrap_or(0.0)
}

/// How much of a live section `full` wide shows at `reveal`; unlimited once
/// all of it shows, so a rough `full` never cuts the pill at rest.
fn reveal_cap(full: f32, reveal: f32) -> f32 {
    if reveal >= 1.0 {
        f32::INFINITY
    } else {
        full * reveal.max(0.0)
    }
}

/// A camera's recent frame rate, with lost frames marked. Until there are
/// enough readings for a trend, the chart's dotted zero line while its
/// rate is `coming` (see `Workbench::rate_coming`), then an empty slot of
/// the same size, so nothing after it moves.
pub(super) fn fps_spark<'a>(
    history: Option<&'a sparkline::History>,
    coming: bool,
    color: Color,
    flag: Color,
    size: (f32, f32),
) -> Element<'a, Message> {
    if !history.is_some_and(sparkline::History::ready) {
        let (width, height) = size;
        if !coming {
            return space().width(width).height(height).into();
        }
        return tip(
            iced::widget::canvas(sparkline::Baseline {
                color: fade(color, 0.55),
            })
            .width(width)
            .height(height),
            "No frame rate yet",
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

    /// The camera state whose dot color is the live end of the pill's tint,
    /// so it agrees with the camera's dot elsewhere. At rest the other
    /// phases show none of it, and a stop fades from it rather than cutting.
    fn state(self) -> CameraState {
        match self {
            Phase::Waiting(_) => CameraState::Stalled,
            _ => CameraState::Live,
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

/// Stable keys for the errors and warnings in `logs`, with their levels, to
/// know each again after the log scrolls: a line's content and how many
/// identical lines came before it, so a repeat of a read problem (failed
/// commands are logged without a time) still counts as new. Should an
/// earlier copy scroll out of the log between reads, one repeat may take
/// its key and go unnoticed.
fn alert_keys(logs: &[LogEntry]) -> impl Iterator<Item = (u64, Level)> + '_ {
    let mut copies: HashMap<u64, u32> = HashMap::new();
    logs.iter().filter_map(move |entry| {
        let level = Level::from_log(&entry.level)?;
        let mut hasher = DefaultHasher::new();
        (&entry.time, &entry.level, &entry.message).hash(&mut hasher);
        let copy = copies.entry(hasher.finish()).or_default();
        *copy += 1;
        copy.hash(&mut hasher);
        Some((hasher.finish(), level))
    })
}

/// Forget read problems beyond this many, keeping those still in the log.
const SEEN_MAX: usize = 512;

/// The worst problem in `logs` not read yet: errors and warnings that came
/// while the log was closed. With the log `open`, everything in it counts as
/// read. Lines are known again by content (see `alert_keys`), so the log may
/// scroll, or switch to another camera's, without a problem counting twice.
fn unseen(logs: &[LogEntry], seen: &mut HashSet<u64>, open: bool) -> Option<Level> {
    let mut worst = None;
    for (key, level) in alert_keys(logs) {
        if open {
            seen.insert(key);
        } else if !seen.contains(&key) && worst != Some(Level::Error) {
            worst = Some(level);
        }
    }
    if seen.len() > SEEN_MAX {
        let present: HashSet<u64> = alert_keys(logs).map(|(key, _)| key).collect();
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

/// Rough width of `text` set at `size`, for deciding what fits. It errs
/// wide, by classes of characters as the system font's semibold sets them,
/// so a title the bar makes room for is never cut short: model names and
/// serials run to capitals and digits, which take well over half an em.
fn rough(text: &str, size: f32) -> f32 {
    let ems: f32 = text
        .chars()
        .map(|c| match c {
            'i' | 'j' | 'l' | 'I' | ' ' | '.' | ',' | ':' | ';' | '\'' | '!' | '|' => 0.25,
            'f' | 'r' | 't' | '/' | '(' | ')' => 0.36,
            '-' => 0.46,
            'm' | 'w' | 'M' => 0.86,
            'W' => 0.96,
            'a'..='z' => 0.55,
            '0'..='9' => 0.64,
            'A'..='Z' => 0.7,
            _ => 1.0,
        })
        .sum();
    ems * size
}

/// What the single-camera title bar keeps; see `Workbench::bar_fit`.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Fit {
    /// The tags (recording, forwarding, scheduled) keep their labels.
    labels: bool,
    /// The status pill keeps its sparkline.
    chart: bool,
    /// The status pill keeps its fps figure.
    figure: bool,
    /// What tells twin cameras apart shows beside the title.
    serial: bool,
    /// The most room the title may take; it ends in "…" beyond it.
    title: f32,
}

/// The status a crowded single-camera bar keeps, from the most to the
/// least, as `(labels, chart, figure, serial)`: tag labels go first, as the
/// tags' glyphs and tooltips still say what they are; then the sparkline,
/// the fps figure, and the twin serial, which the sidebar row still shows.
/// The pill's word and the tags' glyphs always stay, so the title gives way
/// last of all.
const KEEP: [(bool, bool, bool, bool); 5] = [
    (true, true, true, true),
    (false, true, true, true),
    (false, false, true, true),
    (false, false, false, true),
    (false, false, false, false),
];

/// The tag label for `n` active capture jobs.
fn scheduled(n: usize) -> String {
    match n {
        1 => "Scheduled".to_owned(),
        n => format!("{n} scheduled"),
    }
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
            .map_or("", |camera| camera.model.as_str());
        let jobs: Vec<_> = snapshot.jobs.iter().filter(|job| job.active()).collect();
        let output = snapshot.forwarding.as_deref().map(Output::of);
        let scheduled = (!jobs.is_empty()).then(|| scheduled(jobs.len()));
        let tags: Vec<&str> = scheduled
            .as_deref()
            .into_iter()
            .chain(output.as_ref().map(Output::title))
            .collect();
        let fit = self.bar_fit(title, phase.map(Phase::label), &tags);
        let mut status = row![].spacing(6).align_y(Alignment::Center);
        if let Some(phase) = phase {
            let (tint, reveal) = self.live_level();
            let on = phase.state().color(p);
            // Drawn while it folds away too.
            let live = reveal > 0.001;
            let id = snapshot.active_camera.as_deref().unwrap_or_default();
            let history = self.throughput.get(id);
            let coming = self.rate_coming(id);
            let pill = status_pill(
                phase.label(),
                (live && fit.figure).then(|| format!("{:.1} fps", snapshot.fps)),
                Some(FIGURE),
                (live && fit.chart).then(|| fps_spark(history, coming, on, p.warn, (CHART, 14.0))),
                style::mix(p.accent_text, on, tint),
                reveal,
            );
            status = status.push(match phase {
                Phase::Waiting(silent) => tip(pill, silence(silent)),
                _ => pill,
            });
        }
        if let Some(output) = output {
            status = status.push(output_tag(output, fit.labels, p));
        }
        if let Some(label) = scheduled {
            status = status.push(jobs_tag(&jobs, fit.labels.then_some(label), p));
        }
        let heading = snapshot.connected.as_ref().map(|_| {
            let mut heading = row![
                container(one_line(title, style::TITLE, style::BOLD, p.text)).max_width(fit.title)
            ]
            .spacing(10)
            .align_y(Alignment::Center);
            if let Some(identity) = self.twin_identity().filter(|_| fit.serial) {
                heading = heading.push(
                    text(identity)
                        .size(style::BODY)
                        .color(p.secondary)
                        .wrapping(text::Wrapping::None),
                );
            }
            heading.into()
        });
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
        self.title_bar(heading, Some(status.into()), actions.into(), p)
    }

    /// Width of the main area between the panes, as laid out now.
    pub(super) fn main_width(&self) -> f32 {
        let mut main = self.width - self.sidebar_slide.lerp(0.0, SIDEBAR, self.now);
        if self.inspector_docked() {
            main -= self.inspector_slide.lerp(0.0, INSPECTOR, self.now) + 1.0;
        }
        main
    }

    /// What the single-camera bar keeps beside its `title`, from a rough
    /// budget of its width, for a status pill saying `label` and `tags`
    /// with these labels; see `KEEP` for what gives way first. The live
    /// section is budgeted even while it is folded, so starting and
    /// stopping never reflow the bar.
    fn bar_fit(&self, title: &str, label: Option<&str>, tags: &[&str]) -> Fit {
        let snapshot = &self.snapshot;
        let sidebar = self.sidebar_slide.lerp(0.0, SIDEBAR, self.now);
        let padding = 20.0 + (self.lights() - sidebar).max(0.0);
        // The inspector toggle and the gaps before it and the actions; widths
        // are as rendered, give or take a pixel.
        let mut actions = 28.0 + 2.0 * 8.0;
        if label.is_some() {
            // Auto, a gap, and Start or Stop.
            actions += 50.0 + 8.0 + 76.0;
            if self.auto_busy_here() {
                actions += 18.0;
            } else if snapshot.auto.is_some() {
                actions += 12.0;
            }
        }
        if self.shown.is_some() {
            actions += 28.0 + 8.0;
        }
        if snapshot.cameras.len() > 1 {
            actions += 108.0 + 8.0;
        }
        // Less the sidebar toggle and the gaps after it and the heading.
        let room = self.main_width() - padding - actions - 28.0 - 2.0 * 10.0;
        let serial = self
            .twin_identity()
            .map_or(0.0, |identity| 10.0 + rough(&identity, style::BODY));
        // The pill's padding, dot and word.
        let pill = label.map_or(0.0, |label| 32.0 + rough(label, style::SMALL));
        let rest = |(labels, chart, figure, keep_serial): (bool, bool, bool, bool)| {
            let live = if label.is_some() {
                live_width(chart, figure.then_some(""), Some(FIGURE))
            } else {
                0.0
            };
            // Each tag: a gap, padding and its glyph, and any label.
            let tags: f32 = tags
                .iter()
                .map(|tag| {
                    34.0 + if labels {
                        6.0 + rough(tag, style::SMALL)
                    } else {
                        0.0
                    }
                })
                .sum();
            pill + live + tags + if keep_serial { serial } else { 0.0 }
        };
        let whole = rough(title, style::TITLE);
        let keep = KEEP
            .into_iter()
            .find(|&keep| rest(keep) + whole <= room)
            .unwrap_or(KEEP[KEEP.len() - 1]);
        let (labels, chart, figure, serial) = keep;
        Fit {
            labels,
            chart,
            figure,
            serial,
            title: (room - rest(keep)).max(0.0),
        }
    }

    /// While auto mode is busy on the camera shown (see `auto_busy_here`),
    /// what it is doing, for the Auto button's tooltip.
    fn auto_work(&self) -> Option<&'static str> {
        let camera = self.snapshot.active_camera.as_deref()?;
        if self.pending_for(Job::AutoOn, camera) || self.pending_for(Job::AutoOff, camera) {
            Some("Switching auto mode…")
        } else if self.auto_busy_here() {
            Some("Updating auto balance…")
        } else {
            None
        }
    }

    /// Auto for the camera shown: tinted while on, with a dot for how auto
    /// is doing, and the spinner while it switches or takes a new balance.
    fn auto_button(&self, p: &'static Palette) -> Element<'_, Message> {
        let os = Os::CURRENT;
        let status = self.snapshot.auto.as_ref();
        let on = status.is_some();
        let work = self.auto_work();
        let busy = work.is_some();
        let mut label = row![].spacing(6).align_y(Alignment::Center);
        if busy {
            let ink = if on { p.ink(p.accent) } else { p.secondary };
            label = label.push(icon::spinner(12.0, ink, self.spin()));
        } else if let Some(status) = status {
            label = label.push(dot(auto_color(&status.state, p), 6.0));
        }
        label = label.push(text("Auto").size(style::BODY).font(style::MEDIUM));
        let about = if let Some(work) = work {
            work.to_owned()
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
                if self.pending(Job::AutoOn) || self.pending(Job::AutoOff) {
                    "Switching auto mode…"
                } else if auto_busy {
                    "Updating auto balance…"
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
        let heading = one_line("All cameras", style::TITLE, style::BOLD, p.text);
        self.title_bar(Some(heading.into()), Some(status), actions.into(), p)
    }

    /// The row above the main area: pane toggles, what is shown (its
    /// `heading`), its live status and its actions. It folds away in image
    /// mode.
    pub(super) fn title_bar<'a>(
        &self,
        heading: Option<Element<'a, Message>>,
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
        if let Some(heading) = heading {
            left = left.push(heading);
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
fn output_tag<'a>(output: Output, labelled: bool, p: &'static Palette) -> Element<'a, Message> {
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

/// The camera's active capture jobs, with any `label`; opens the Capture
/// tab.
fn jobs_tag<'a>(
    jobs: &[&CaptureJob],
    label: Option<String>,
    p: &'static Palette,
) -> Element<'a, Message> {
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
        label,
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
        // But fades from live, not from idle.
        assert_eq!(Phase::Stopping.state(), CameraState::Live);
        // Start all and Stop all reach the camera shown too.
        bench.pending.clear();
        bench.snapshot.streaming = false;
        bench.send_batch(vec!["sim:0".into()], Job::Start, || SessionCommand::Start);
        assert_eq!(bench.single_phase(), Some(Phase::Starting));
        bench.pending.clear();
        bench.snapshot.streaming = true;
        bench.send_batch(vec!["sim:0".into()], Job::Stop, || SessionCommand::Stop);
        assert_eq!(bench.single_phase(), Some(Phase::Stopping));
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
        assert_eq!(unseen(&logs, &mut seen, true), None);
        // The same command fails again: a new line, though not new words.
        logs.push(log("", "error", "the camera stopped responding"));
        assert_eq!(unseen(&logs, &mut seen, false), Some(Level::Error));
        assert_eq!(unseen(&logs, &mut seen, true), None);
        assert_eq!(unseen(&logs, &mut seen, false), None);
    }

    /// The bench with a second camera of the same model, so the title bar
    /// says which one is shown, and both panes open.
    fn twins() -> Workbench {
        let mut bench = bench();
        let mut twin = streaming_camera(10, 0, 30.0);
        twin.info.id = "sim:1".into();
        twin.info.serial = "SIM1".into();
        bench.snapshot.cameras.push(twin);
        bench.sidebar_slide.set(1.0);
        bench.inspector_slide.set(1.0);
        bench
    }

    #[test]
    fn crowded_bars_give_way_in_order() {
        let mut bench = twins();
        let title = "Pattern camera";
        let whole = rough(title, style::TITLE);
        let both = ["Recording", "Scheduled"];
        let keeps = |fit: Fit| (fit.labels, fit.chart, fit.figure, fit.serial);
        bench.width = 1440.0;
        let one = bench.bar_fit(title, Some("Streaming"), &both[..1]);
        assert_eq!(keeps(one), (true, true, true, true), "room for a label");
        let two = bench.bar_fit(title, Some("Streaming"), &both);
        assert_eq!(keeps(two), (false, true, true, true), "the labels go first");
        assert!(two.title >= whole);
        bench.width = 1320.0;
        let fit = bench.bar_fit(title, Some("Waiting for frames"), &both);
        assert_eq!(
            keeps(fit),
            (false, false, false, true),
            "then the live section"
        );
        // The smallest window: the inspector floats, and the serial goes too.
        bench.width = 900.0;
        let fit = bench.bar_fit(title, Some("Waiting for frames"), &both);
        assert_eq!(keeps(fit), (false, false, false, false));
        assert!(fit.title > 0.0, "the word and glyphs fit");
        // Whatever the width, things go in order, and the title gives way
        // last, model names of capitals and digits included.
        for title in [title, "MV-CA050-10GC", "mvBlueCOUGAR-X104dG"] {
            let whole = rough(title, style::TITLE);
            for width in (900..=1800).step_by(10) {
                bench.width = width as f32;
                for label in ["Ready", "Streaming", "Waiting for frames"] {
                    let fit = bench.bar_fit(title, Some(label), &both);
                    let (labels, chart, figure, serial) = keeps(fit);
                    assert!(!labels || chart, "{width}: {fit:?}");
                    assert!(!chart || figure, "{width}: {fit:?}");
                    assert!(!figure || serial, "{width}: {fit:?}");
                    assert!(fit.title >= whole || !serial, "{width}: {fit:?}");
                }
            }
        }
        // Errs wide of the names as set in the title's semibold.
        for (name, set) in [
            ("Pattern camera", 100.7),
            ("MV-CA050-10GC", 121.9),
            ("mvBlueCOUGAR-X104dG", 174.5),
            ("BFS-PGE-31S4C-C", 130.6),
        ] {
            assert!(rough(name, style::TITLE) >= set, "{name}");
        }
        assert!(rough("S/N 0123ABCD4567", style::BODY) >= 116.1);
        // Alone, with the sidebar closed, everything fits.
        bench.snapshot.cameras.truncate(1);
        bench.sidebar_slide.set(0.0);
        bench.width = 960.0;
        let fit = bench.bar_fit(title, Some("Streaming"), &both);
        assert_eq!(keeps(fit), (true, true, true, true));
    }

    #[test]
    fn the_live_section_uncovers_at_its_own_width() {
        assert_eq!(
            live_width(true, Some("30.0 fps"), Some(FIGURE)),
            6.0 + CHART + 6.0 + FIGURE
        );
        assert_eq!(
            live_width(false, Some("30.0 fps"), Some(FIGURE)),
            6.0 + FIGURE
        );
        assert_eq!(live_width(false, None, Some(FIGURE)), 0.0);
        // The overview's count: only as wide as it is.
        let full = live_width(false, Some("2 of 4"), None);
        assert!(full > 6.0 && full < 6.0 + FIGURE);
        let mut bench = bench();
        let start = bench.now;
        bench.snapshot.streaming = true;
        bench.sync_animations();
        // The reveal before Reduce Motion, which snaps it.
        let reveal = |bench: &Workbench| bench.chrome.live.get(bench.now);
        let steps = 8;
        let mut last = 0.0;
        for step in 1..steps {
            bench.now = start + motion::SLIDE * step / steps;
            let cap = reveal_cap(full, reveal(&bench));
            assert!(cap > last && cap < full, "grows all through the entrance");
            last = cap;
        }
        bench.now = start + motion::SLIDE;
        assert_eq!(
            reveal_cap(full, reveal(&bench)),
            f32::INFINITY,
            "unlimited at rest"
        );
        let stop = bench.now;
        bench.snapshot.streaming = false;
        bench.sync_animations();
        let mut last = full;
        for step in 1..steps {
            bench.now = stop + motion::SLIDE_OUT * step / steps;
            let cap = reveal_cap(full, reveal(&bench));
            assert!(cap < last && cap > 0.0, "shrinks all through the exit");
            last = cap;
        }
    }

    #[test]
    fn auto_shows_only_the_shown_camera_at_work() {
        let mut bench = twins();
        assert_eq!(bench.auto_work(), None);
        // Another camera's auto command neither spins nor holds up this one.
        bench.pending.push(Pending {
            camera: Some("sim:1".into()),
            ..Pending::unanswered(Job::AutoOn)
        });
        assert!(bench.auto_busy());
        assert!(!bench.auto_busy_here());
        assert_eq!(bench.auto_work(), None);
        // A new balance is not a switch.
        bench.pending.push(Pending {
            camera: Some("sim:0".into()),
            ..Pending::unanswered(Job::Balance)
        });
        assert!(bench.auto_busy_here());
        assert_eq!(bench.auto_work(), Some("Updating auto balance…"));
        bench.pending.push(Pending {
            camera: Some("sim:0".into()),
            ..Pending::unanswered(Job::AutoOff)
        });
        assert_eq!(bench.auto_work(), Some("Switching auto mode…"));
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
