//! The quick guide sheet.
use super::*;
use iced::widget::column;

/// The sheets package's own state: the quick guide and the destination sheets
/// (whose pickers keep their own, see `Picker`). Add fields here, register
/// their motions below and point them in `sync_sheets`.
pub(super) struct SheetState {
    /// The open sheet and its scrim rising in: 1 shown, 0 gone.
    pub(super) shown: Motion,
}

impl Default for SheetState {
    fn default() -> Self {
        Self {
            shown: Motion::new(0.0, motion::SHEET, motion::SHEET_OUT, Kind::Fade),
        }
    }
}

impl SheetState {
    super::motion::registry! {
        motions: [shown],
        flashes: [],
    }
}

/// The sheets package's hooks into the shared update cycle; empty until it
/// needs them.
impl Workbench {
    /// Point the sheets package's motions at what they show; from
    /// `sync_animations`. Closing snaps: nothing draws a sheet on its way out
    /// yet, so a fade would only run frames, and the next sheet would rise from
    /// partway.
    pub(super) fn sync_sheets(&mut self) {
        if self.sheet_open() {
            self.sheets.shown.go(1.0, self.now);
        } else {
            self.sheets.shown.set(0.0);
        }
    }

    /// The sheets package's bookkeeping on the slow tick, after the snapshot
    /// refresh; from `tick()`.
    pub(super) fn tick_sheets(&mut self) {}

    /// A command finished, after the shared bookkeeping (`finished`,
    /// `failed`) and before its notice; from `settle()`.
    pub(super) fn result_sheets(
        &mut self,
        _pending: &Pending,
        _result: &anyhow::Result<serde_json::Value>,
    ) {
    }

    /// Take a screenshot scene word the sheets package owns: `late` is false
    /// while the scene is set up and true once its cameras stream. Returns
    /// whether the word was taken; see `apply_scene`.
    pub(super) fn scene_sheets(&mut self, _word: &str, _late: bool) -> bool {
        false
    }
}

impl Workbench {
    pub(super) fn help(&self, p: &'static Palette) -> Element<'_, Message> {
        let os = Os::CURRENT;
        let section = |(name, actions): (&'static str, &'static [Action])| {
            let mut list = column![heading(name, p)].spacing(7);
            for action in actions
                .iter()
                .filter(|action| !action.bindings(os).is_empty())
            {
                list = list.push(
                    row![
                        text(action.label()).size(style::BODY).width(Fill),
                        text(action.shortcut(os))
                            .size(style::SMALL)
                            .font(style::MEDIUM)
                            .color(p.secondary),
                    ]
                    .spacing(10),
                );
            }
            list
        };
        let [cameras, acquisition, view, window] = Action::SECTIONS;
        let content = column![
            row![
                icon(Icon::Keyboard, 24.0, p.accent),
                text("Quick guide").size(style::DISPLAY).font(style::BOLD),
                space::horizontal(),
                tip(
                    button(icon(Icon::Close, 14.0, p.secondary))
                        .padding(6)
                        .style(style::plain)
                        .on_press(Message::Help(false)),
                    Action::Overview.hint("Close", os),
                ),
            ]
            .spacing(10)
            .align_y(Alignment::Center),
            text("Discover a camera, connect, then start a stream. The simulator is available without camera hardware.")
                .size(style::BODY)
                .color(p.secondary),
            row![
                column![section(cameras), section(acquisition)].spacing(18).width(Fill),
                column![section(view), section(window)].spacing(18).width(Fill),
            ]
            .spacing(32),
            text(format!(
                "{}, {} and {} act on the workbench when no field has keyboard focus. Press {} or click elsewhere to leave a field.",
                Action::ToggleStream.shortcut(os),
                Action::FocusCamera.shortcut(os),
                Action::Overview.shortcut(os),
                Action::Overview.shortcut(os)
            ))
                .size(style::SMALL)
                .color(p.secondary),
            heading("One visible session, many ways to control it", p),
            text("Copy the session command to attach your terminal or coding agent. Session commands update this window and share its camera connection.")
                .size(style::BODY),
            self.copyable(self.session_command(), p),
            self.copyable("capturefab --help".into(), p),
            text("Add stream URLs directly. Native camera sources use avfoundation:// on macOS, v4l2:// on Linux, and dshow:// on Windows. GigE: use a reachable address on the camera's subnet. USB3: the operating system must allow access to the camera. Activity shows connection and capture errors.")
                .size(style::SMALL)
                .color(p.secondary),
        ]
        .spacing(14)
        .padding(28);
        container(scrollable(content).style(style::scroll))
            .width(700)
            .max_height(680)
            .style(style::sheet)
            .into()
    }
}
