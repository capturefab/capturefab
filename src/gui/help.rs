//! The sheets over the workbench: the quick guide here, and the destination
//! manager whose content `Picker` draws. Only one sheet shows at a time.
use super::*;
use iced::widget::column;

/// Room kept clear above and below a sheet; it scrolls inside the rest.
const SHEET_MARGIN: f32 = 40.0;
/// The least height a sheet gets, however short the window.
const SHEET_MIN: f32 = 320.0;

/// State for the sheets: the quick guide and the destination sheets (whose
/// pickers keep their own, see `Picker`).
pub(super) struct SheetState {
    /// The scrim of a sheet: 1 shown, 0 gone. It rises with the sheet and
    /// fades out after the sheet closes, so the window never flashes back
    /// to full brightness.
    pub(super) shown: Motion,
    /// The guide's body has scrolled under its header.
    pub(super) scrolled: bool,
    /// The guide's "Connecting cameras" notes are open.
    pub(super) more: bool,
}

impl Default for SheetState {
    fn default() -> Self {
        Self {
            shown: Motion::new(0.0, motion::SHEET, motion::SHEET_OUT, Kind::Fade),
            scrolled: false,
            more: false,
        }
    }
}

impl SheetState {
    super::motion::registry! {
        motions: [shown],
        flashes: [],
    }
}

/// The sheets' hooks into the shared update cycle.
impl Workbench {
    /// Point the sheets' motions at what they show; from
    /// `sync_animations`. The scrim fades in with a sheet and out after it;
    /// a sheet opening while the scrim still fades out takes over from there.
    pub(super) fn sync_sheets(&mut self) {
        self.sheets.shown.show(self.sheet_open(), self.now);
    }

    /// Take a screenshot scene word for the sheets: `late` is false
    /// while the scene is set up and true once its cameras stream. Returns
    /// whether the word was taken; see `apply_scene`. Words: `help-more`
    /// (the guide with its connection notes open), and the destination
    /// sheet's `destinations-list`, `folder-editor`, `bucket-editor` and
    /// `testing` (see `Picker::scene`).
    pub(super) fn scene_sheets(&mut self, word: &str, late: bool) -> bool {
        if late {
            return false;
        }
        match word {
            "help-more" => {
                self.help_open = true;
                self.sheets.more = true;
                true
            }
            word => self.capture_to.scene(word),
        }
    }
}

impl Workbench {
    /// Open or close the guide. It waits while another sheet is open, so
    /// two sheets never stack.
    pub(super) fn show_help(&mut self, open: bool) {
        if open == self.help_open || (open && self.sheet_open()) {
            return;
        }
        self.help_open = open;
        if open {
            self.sheets.scrolled = false;
            self.sheets.more = false;
        }
    }

    /// Hand a message to the capture (`recording` false) or recording
    /// destination picker, focusing the first field of an editor it opens.
    pub(super) fn picker(
        &mut self,
        recording: bool,
        message: destinations::Message,
    ) -> Task<Message> {
        let (picker, output) = if recording {
            (&mut self.record_to, &mut self.forward_output)
        } else {
            (&mut self.capture_to, &mut self.output)
        };
        picker.update(message, output);
        match picker.take_focus() {
            Some(id) => Task::batch([operation::focus(id), operation::select_all(id)]),
            None => Task::none(),
        }
    }

    /// The tallest a sheet may be: the window less a margin.
    fn sheet_height(&self) -> f32 {
        (self.height - 2.0 * SHEET_MARGIN).max(SHEET_MIN)
    }

    /// `body` with the open sheet over it; after one closes, the scrim alone
    /// fading out, taking no input.
    pub(super) fn sheets<'a>(
        &'a self,
        body: Element<'a, Message>,
        p: &'static Palette,
    ) -> Element<'a, Message> {
        let t = self.sheets.shown.get(self.now);
        let (height, spin, now) = (self.sheet_height(), self.spin(), self.now);
        if self.help_open {
            modal(body, self.help(p, height), Some(Message::Help(false)), t)
        } else if self.capture_to.manager_open {
            modal(
                body,
                self.capture_to
                    .manager(p.dark, height, spin, now)
                    .map(Message::CapturePicker),
                (!self.capture_to.editing())
                    .then_some(Message::CapturePicker(destinations::Message::CloseManager)),
                t,
            )
        } else if self.record_to.manager_open {
            modal(
                body,
                self.record_to
                    .manager(p.dark, height, spin, now)
                    .map(Message::RecordPicker),
                (!self.record_to.editing())
                    .then_some(Message::RecordPicker(destinations::Message::CloseManager)),
                t,
            )
        } else if t > 0.0 {
            stack![
                body,
                container(space().width(Fill).height(Fill))
                    .style(move |theme| style::scrim(theme, t)),
            ]
            .into()
        } else {
            body
        }
    }

    fn help(&self, p: &'static Palette, height: f32) -> Element<'_, Message> {
        let os = Os::CURRENT;
        let section = |(name, actions): (&'static str, &'static [Action])| {
            let mut list = column![heading(name, p)].spacing(7);
            for action in actions
                .iter()
                .filter(|action| action.display_binding(os).is_some())
            {
                list = list.push(
                    row![
                        text(action.label()).size(style::BODY).width(Fill),
                        text(action.key_label(os))
                            .size(style::SMALL)
                            .font(style::MEDIUM)
                            .color(p.secondary),
                    ]
                    .spacing(10),
                );
            }
            list
        };
        let [cameras, acquisition, view, panels, window] = Action::SECTIONS;
        let more = self.sheets.more;
        let mut body = column![
            text("Discover a camera, connect, then start a stream. The simulator is available without camera hardware.")
                .size(style::BODY)
                .color(p.secondary),
            row![
                column![section(cameras), section(acquisition), section(window)]
                    .spacing(18)
                    .width(Fill),
                column![section(view), section(panels)].spacing(18).width(Fill),
            ]
            .spacing(32),
            text(format!(
                "{}, {} and {} act on the workbench when no field has keyboard focus. Press {} or click elsewhere to leave a field.",
                Action::ToggleStream.key_label(os),
                Action::FocusCamera.key_label(os),
                Action::Overview.key_label(os),
                Action::Overview.key_label(os)
            ))
            .size(style::SMALL)
            .color(p.secondary),
            heading("Control this window from a terminal or agent", p),
            text("Copy the session command to attach your terminal or coding agent. Session commands update this window and share its camera connection.")
                .size(style::BODY),
            self.copyable(self.session_command(), p),
            self.copyable("capturefab --help".into(), p),
            disclosure(
                "Connecting cameras",
                None,
                if more { 1.0 } else { 0.0 },
                Message::HelpMore(!more),
                p,
            ),
        ]
        .spacing(14);
        if more {
            body = body.push(
                text("Add stream URLs directly. Native camera sources use avfoundation:// on macOS, v4l2:// on Linux, and dshow:// on Windows. GigE: use a reachable address on the camera's subnet. USB3: the operating system must allow access to the camera. Activity shows connection and capture errors.")
                    .size(style::SMALL)
                    .color(p.secondary),
            );
        }
        sheet_frame("Quick guide", Message::Help(false), body, p)
            .mark(Icon::Keyboard)
            .size(700.0, height)
            .scrolled(self.sheets.scrolled)
            .on_scroll(Message::HelpScrolled)
            .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bench() -> Workbench {
        Workbench::new(SessionHandle::new(), "test".into(), true, None)
    }

    fn press(bench: &mut Workbench, action: Action) {
        let chord = action.bindings(Os::CURRENT)[0];
        let _ = bench.shortcut(chord, true);
    }

    #[test]
    fn the_guide_waits_while_another_sheet_is_open() {
        let mut bench = bench();
        bench.capture_to.manager_open = true;
        press(&mut bench, Action::Help);
        assert!(!bench.help_open, "the shortcut");
        let _ = bench.handle_message(Message::Help(true));
        assert!(!bench.help_open, "a button");
        // Esc closes the sheet that shows.
        press(&mut bench, Action::Overview);
        assert!(!bench.capture_to.manager_open);
        press(&mut bench, Action::Help);
        assert!(bench.help_open);
        press(&mut bench, Action::Help);
        assert!(!bench.help_open, "the shortcut toggles it");
    }

    #[test]
    fn the_guide_opens_at_its_top_with_its_notes_closed() {
        let mut bench = bench();
        let _ = bench.handle_message(Message::Help(true));
        let _ = bench.handle_message(Message::HelpScrolled(true));
        let _ = bench.handle_message(Message::HelpMore(true));
        assert!(bench.sheets.scrolled && bench.sheets.more);
        let _ = bench.handle_message(Message::Help(false));
        let _ = bench.handle_message(Message::Help(true));
        assert!(!bench.sheets.scrolled && !bench.sheets.more);
    }

    #[test]
    fn sheets_follow_the_window_height() {
        let mut bench = bench();
        bench.height = 900.0;
        assert_eq!(bench.sheet_height(), 820.0);
        bench.height = 300.0;
        assert_eq!(bench.sheet_height(), SHEET_MIN);
    }

    #[test]
    fn the_scrim_fades_out_after_a_sheet_and_settles() {
        let mut bench = bench();
        let start = bench.now;
        bench.help_open = true;
        bench.sync_animations();
        bench.now = start + motion::SHEET;
        assert_eq!(bench.sheets.shown.get(bench.now), 1.0);
        assert!(!bench.sheets.shown.animating(bench.now));
        let closed = bench.now;
        bench.help_open = false;
        bench.sync_animations();
        bench.now = closed + motion::SHEET_OUT / 2;
        let half = bench.sheets.shown.get(bench.now);
        assert!(half > 0.0 && half < 1.0, "fading: {half}");
        assert!(bench.animating(), "redraws while the scrim fades");
        // A sheet opening mid-fade takes over from where the scrim is.
        bench.capture_to.manager_open = true;
        bench.sync_animations();
        assert!((bench.sheets.shown.get(bench.now) - half).abs() < 1e-4);
        bench.capture_to.manager_open = false;
        bench.sync_animations();
        bench.now += motion::SHEET_OUT;
        assert_eq!(bench.sheets.shown.get(bench.now), 0.0);
        assert!(!bench.sheets.shown.animating(bench.now), "settles");
    }
}
