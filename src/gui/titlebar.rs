//! The title bar over the main area: pane toggles, what is shown, its live
//! status and its actions.
use super::*;

/// The chrome package's own state: the title bar and the toolbar. Add fields
/// here, register their motions below and point them in `sync_chrome`. It
/// derives `Default` while empty; write `Default` by hand once it holds a
/// `Motion`.
#[derive(Default)]
pub(super) struct ChromeState {}

impl ChromeState {
    super::motion::registry! {
        motions: [],
        flashes: [],
    }
}

/// The chrome package's hooks into the shared update cycle; empty until it
/// needs them.
impl Workbench {
    /// Point the chrome package's motions at what they show; from
    /// `sync_animations`.
    pub(super) fn sync_chrome(&mut self) {}

    /// The chrome package's bookkeeping on the slow tick, after the snapshot
    /// refresh; from `tick()`.
    pub(super) fn tick_chrome(&mut self) {}

    /// A command finished, after the shared bookkeeping (`finished`,
    /// `failed`) and before its notice; from `settle()`.
    pub(super) fn result_chrome(
        &mut self,
        _pending: &Pending,
        _result: &anyhow::Result<serde_json::Value>,
    ) {
    }

    /// Take a screenshot scene word the chrome package owns: `late` is false
    /// while the scene is set up and true once its cameras stream. Returns
    /// whether the word was taken; see `apply_scene`.
    pub(super) fn scene_chrome(&mut self, _word: &str, _late: bool) -> bool {
        false
    }
}

/// Width of a status pill's live section at rest: the sparkline, a gap and
/// the figure's slot.
const LIVE: f32 = 64.0 + 6.0 + 58.0;

/// A rounded status label: a steady dot, a word, and an optional live
/// section of a `chart` and a `figure`. With a `slot`, the figure keeps that
/// fixed width, so a changing figure never shifts what follows. `reveal`
/// shows the live section, from 0 (hidden) to 1 (all of it), for animating
/// the change into and out of streaming.
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
        text(label)
            .size(style::SMALL)
            .font(style::MEDIUM)
            .wrapping(text::Wrapping::None),
    ]
    .spacing(6)
    .align_y(Alignment::Center);
    let mut live = row![].spacing(6).align_y(Alignment::Center);
    if let Some(chart) = chart {
        live = live.push(chart);
    }
    if let Some(figure) = figure {
        let figure = text(figure)
            .size(style::SMALL)
            .wrapping(text::Wrapping::None);
        live = live.push(match slot {
            Some(width) => figure.width(width),
            None => figure,
        });
    }
    if reveal >= 0.999 {
        content = content.push(live);
    } else if reveal > 0.001 {
        content = content.push(container(live).max_width(LIVE * reveal).clip(true));
    }
    container(content)
        .padding([3, 10])
        .style(style::pill(color))
        .into()
}

/// A camera's recent frame rate, with lost frames marked.
pub(super) fn fps_spark<'a>(
    history: Option<&'a sparkline::History>,
    color: Color,
    flag: Color,
    size: (f32, f32),
) -> Element<'a, Message> {
    spark(history, color, flag, size, |history| {
        trend(
            history,
            "Frame rate",
            |fps| format!("{fps:.1} fps"),
            "lost frames",
        )
    })
}

impl Workbench {
    /// The single-camera title bar: the camera's model, its live status, and
    /// Auto, Start/Stop and the way back to all cameras.
    pub(super) fn single_bar(&self, p: &'static Palette) -> Element<'_, Message> {
        let snapshot = &self.snapshot;
        let connected = snapshot.connected.is_some();
        let status = if snapshot.streaming {
            let history = snapshot
                .connected
                .as_ref()
                .and_then(|camera| self.throughput.get(&camera.id));
            Some(status_pill(
                "Streaming",
                Some(format!("{:.1} fps", snapshot.fps)),
                Some(58.0),
                Some(fps_spark(history, p.live, p.warn, (64.0, 14.0))),
                p.live,
                1.0,
            ))
        } else if connected {
            Some(status_pill("Ready", None, None, None, p.accent_text, 1.0))
        } else {
            None
        };
        let os = Os::CURRENT;
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
                .padding([5, 10])
                .style(style::secondary)
                .on_press(Message::Focus(false)),
                Action::Overview.hint("Back to all cameras", os),
            ));
        }
        if connected {
            let auto = snapshot.auto.is_some();
            let busy = snapshot.active_camera.as_deref().is_some_and(|id| {
                self.pending_for("Starting stream", id) || self.pending_for("Stopping stream", id)
            });
            actions = actions
                .push(tip(
                    button(text("Auto").size(style::BODY).font(style::MEDIUM))
                        .padding([5, 12])
                        .style(style::toggle(auto))
                        .on_press_maybe((!self.auto_busy()).then_some(Message::Auto(!auto))),
                    Action::ToggleAuto.hint("Tune exposure, gain and frame rate", os),
                ))
                .push(tip(
                    stream_button(
                        snapshot.streaming,
                        if snapshot.streaming { "Stop" } else { "Start" },
                        Some(Message::ToggleStream),
                        busy,
                        self.spin(),
                        p,
                    ),
                    Action::ToggleStream.hint("Start or stop acquisition", os),
                ));
        }
        let title = snapshot
            .connected
            .as_ref()
            .map_or(String::new(), |camera| camera.model.clone());
        self.title_bar(title, status, actions.into(), p)
    }

    /// The overview's title bar: how many cameras stream, and actions for
    /// all of them.
    pub(super) fn overview_bar(&self, p: &'static Palette) -> Element<'_, Message> {
        let snapshot = &self.snapshot;
        let streaming = snapshot.cameras.iter().filter(|c| c.streaming).count();
        let status = status_pill(
            if streaming > 0 { "Streaming" } else { "Ready" },
            Some(format!("{streaming} of {}", snapshot.cameras.len())),
            Some(58.0),
            None,
            if streaming > 0 { p.live } else { p.accent_text },
            1.0,
        );
        let start = snapshot.cameras.iter().any(|camera| !camera.streaming);
        let manual = snapshot.cameras.iter().any(|camera| camera.auto.is_none());
        let busy = self.pending("Starting streams") || self.pending("Stopping streams");
        let actions = row![
            button(
                text(if manual { "Auto all" } else { "Manual all" })
                    .size(style::BODY)
                    .font(style::MEDIUM)
            )
            .padding([5, 12])
            .style(style::toggle(!manual))
            .on_press(Message::AllAuto(manual)),
            stream_button(
                !start,
                if start { "Start all" } else { "Stop all" },
                Some(Message::AllStreams(start)),
                busy,
                self.spin(),
                p,
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
        }
        if let Some(status) = status {
            left = left.push(status);
        }
        let bar = row![
            container(left).width(Fill).clip(true),
            actions,
            tip(
                icon_button(Icon::CornersOut, 16.0, p.secondary, Message::ToggleImage),
                Action::ImageMode.hint("Image only", os),
            ),
            tip(
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
            ),
        ]
        .spacing(8)
        .align_y(Alignment::Center);
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
}
