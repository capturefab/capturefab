//! The sheets over the workbench: the quick guide here, and the destination
//! manager whose content `Picker` draws. Only one sheet shows at a time.
use super::*;
use iced::widget::column;

/// Room kept clear above and below a sheet; it scrolls inside the rest.
const SHEET_MARGIN: f32 = 40.0;
/// The least height a sheet gets, however short the window.
const SHEET_MIN: f32 = 320.0;

/// Snap the sheet's body to its top when `picker` changed pages: to iced
/// the list and an editor are one sheet, which would keep the scroll.
fn page_top(picker: &mut Picker) -> Task<Message> {
    picker_page(picker, true)
}

fn picker_focus(picker: &mut Picker, was_open: bool, page_changed: bool) -> Option<&'static str> {
    picker.take_focus().or_else(|| {
        (picker.manager_open && (!was_open || (page_changed && !picker.editor_open())))
            .then_some("sheet-close")
    })
}

fn picker_page(picker: &mut Picker, was_open: bool) -> Task<Message> {
    let changed = picker.take_to_top();
    let focus = picker_focus(picker, was_open, changed);
    Task::batch([
        if changed {
            operation::snap_to(SHEET_BODY, operation::RelativeOffset::START)
        } else {
            Task::none()
        },
        focus.map_or_else(Task::none, |id| {
            Task::batch([operation::focus(id), operation::select_all(id)])
        }),
    ])
}

/// State for the sheets: the quick guide and the destination sheets (whose
/// pickers keep their own, see `Picker`).
pub(super) struct SheetState {
    /// The guide is showing the external-agent connection page.
    pub(super) agents: bool,
    /// The scrim of a sheet: 1 shown, 0 gone. It rises with the sheet and
    /// fades out after the sheet closes, so the window never flashes back
    /// to full brightness.
    pub(super) shown: Motion,
    /// The guide's body has scrolled under its header.
    pub(super) scrolled: bool,
    /// The guide's "Connecting cameras" notes are open.
    pub(super) more: bool,
    /// The notes' chevron: 0 closed, 1 open.
    pub(super) more_turn: Motion,
    /// The chevron of Settings' API keys section: 0 folded, 1 open.
    pub(super) keys_turn: Motion,
}

impl Default for SheetState {
    fn default() -> Self {
        Self {
            agents: false,
            shown: Motion::new(0.0, motion::SHEET, motion::SHEET_OUT, Kind::Fade),
            scrolled: false,
            more: false,
            more_turn: Motion::new(0.0, motion::RING, motion::RING_OUT, Kind::Move),
            keys_turn: Motion::new(0.0, motion::RING, motion::RING_OUT, Kind::Move),
        }
    }
}

impl SheetState {
    super::motion::registry! {
        motions: [shown, more_turn, keys_turn],
        flashes: [],
    }
}

/// The sheets' hooks into the shared update cycle.
impl Workbench {
    /// Point the sheets' motions at what they show; from
    /// `sync_animations`. The scrim fades in with a sheet and out after it;
    /// a sheet opening while the scrim still fades out takes over from there.
    pub(super) fn sync_sheets(&mut self) {
        let now = self.now;
        self.sheets.shown.show(self.sheet_open(), now);
        self.sheets.more_turn.show(self.sheets.more, now);
        // The keys section turns only while Settings shows; otherwise, as
        // at launch with it remembered open, it is already where it rests.
        let keys = self.video_analytics.keys_section_open();
        if self.settings_open {
            self.sheets.keys_turn.show(keys, now);
        } else {
            self.sheets.keys_turn.set(if keys { 1.0 } else { 0.0 });
        }
        self.capture_to.sync(now);
        self.record_to.sync(now);
    }

    /// Take a screenshot scene word for the sheets: `late` is false
    /// while the scene is set up and true once its cameras stream. Returns
    /// whether the word was taken; see `apply_scene`. Words: `help-more`
    /// (the guide with its connection notes open), and the destination
    /// sheet's `destinations-list`, `folder-editor`, `folder-error` and
    /// `testing` (see `Picker::scene`).
    pub(super) fn scene_sheets(&mut self, word: &str, late: bool) -> bool {
        if late {
            return false;
        }
        match word {
            "agent-setup" => {
                self.show_agent_setup(true);
                true
            }
            "settings" => {
                self.settings_open = true;
                true
            }
            "help-more" => {
                self.help_open = true;
                self.sheets.more = true;
                self.sheets.more_turn.set(1.0);
                true
            }
            word => self.capture_to.scene(word, &mut self.output),
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
            self.sheets.agents = false;
            self.sheets.scrolled = false;
            self.sheets.more = false;
            // Closed already: no turn while the sheet fades in.
            self.sheets.more_turn.set(0.0);
        }
    }

    /// Open or close the guide's notes on connecting cameras. Opening them
    /// from elsewhere, as the welcome screen's "Connection tips" does, opens
    /// the guide too, unless another sheet is open. They sit under the
    /// guide's first steps, so they open in view.
    pub(super) fn show_help_notes(&mut self, open: bool) -> Task<Message> {
        if open && !self.help_open {
            self.show_help(true);
            if !self.help_open {
                return Task::none();
            }
            // `show_help` closes the notes; they open as the sheet does.
            self.sheets.more = true;
            self.sheets.more_turn.set(1.0);
            return operation::focus("sheet-close");
        }
        self.sheets.more = open;
        Task::none()
    }

    /// Which of the guide's first steps are done: a camera connected, a
    /// stream started, a frame captured, then a series: several frames in
    /// one capture, or a scheduled capture. That one stays done once it was.
    pub(super) fn guide_steps(&self) -> [bool; 4] {
        [
            !self.snapshot.cameras.is_empty(),
            self.snapshot.cameras.iter().any(|camera| camera.streaming),
            self.last_saved.is_some(),
            self.captured_series
                || self
                    .last_saved
                    .as_ref()
                    .is_some_and(|saved| saved.count > 1),
        ]
    }

    /// The guide's next step after one frame: close it and put the
    /// keyboard in the Capture panel's frame count.
    pub(super) fn set_frame_count(&mut self) -> Task<Message> {
        if self.help_open {
            self.show_help(false);
        }
        self.tab = Tab::Capture;
        self.show_sidebar();
        let id = format!("number:{:?}", Num::Count);
        Task::batch([operation::focus(id.clone()), operation::select_all(id)])
    }

    /// The guide's first steps, each marked once done; the first one not
    /// done offers what does it.
    fn guide_steps_view(&self, p: &'static Palette) -> Element<'_, Message> {
        let done = self.guide_steps();
        let next = done.iter().position(|done| !done);
        let os = Os::CURRENT;
        // Small buttons, the shortcut beside the label, so the next step
        // reads as something to press.
        let link = |label: &'static str, name: &'static str, on: Option<Message>| {
            step_button(label, None, name, on, p)
        };
        let keyed = |label: &'static str, key: Action, name: &'static str, on: Option<Message>| {
            step_button(label, Some(key.key_label(os)), name, on, p)
        };
        let mut steps = column![].spacing(4);
        for (index, label) in [
            "Connect a camera",
            "Start the stream",
            "Capture a frame",
            "Capture a series",
        ]
        .into_iter()
        .enumerate()
        {
            let mark: Element<'_, Message> = if done[index] {
                icon(Icon::Check, 14.0, p.ink(p.live))
            } else {
                text((index + 1).to_string())
                    .size(style::SMALL)
                    .font(style::SEMIBOLD)
                    .color(p.secondary)
                    .into()
            };
            // The mark starts at the content edge, under the headings.
            let mut step = row![
                container(mark)
                    .width(18)
                    .height(26)
                    .align_x(Alignment::Start)
                    .align_y(Alignment::Center),
                text(label)
                    .size(style::BODY)
                    .color(if done[index] { p.secondary } else { p.text }),
            ]
            .spacing(8)
            .align_y(Alignment::Center);
            if next == Some(index) {
                step = step.push(space().width(4));
                match index {
                    0 => {
                        step = step.push(keyed(
                            "Enter an address",
                            Action::ConnectAddress,
                            "Enter an address",
                            Some(Message::EnterAddress),
                        ));
                        // A simulated camera already listed needs no offer;
                        // with cameras listed, picking one is the next step.
                        if !welcome::sim_listed(self.snapshot.devices.iter(), self.recent.iter()) {
                            step = step.push(link(
                                "No camera? Try the simulator",
                                "Try the simulator",
                                (!self.connecting("sim:0")).then_some(Message::TrySimulator),
                            ));
                        } else if !self.snapshot.devices.is_empty() {
                            step = step.push(
                                text("or pick one on the left")
                                    .size(style::SMALL)
                                    .color(p.secondary),
                            );
                        }
                    }
                    1 => {
                        step = step.push(keyed(
                            "Start",
                            Action::ToggleStream,
                            "Start the stream",
                            self.snapshot
                                .active_camera
                                .is_some()
                                .then_some(Message::ToggleStream),
                        ));
                    }
                    2 => {
                        step = step.push(keyed(
                            "Capture",
                            Action::Capture,
                            "Capture a frame",
                            (!self.capturing_here()
                                && self.validate_input(&self.capture_command(false)).is_ok())
                            .then_some(Message::Capture),
                        ));
                    }
                    _ => {
                        // In the Capture panel's words for this camera.
                        let label = match self.camera_kind() {
                            CameraKind::DocumentScanner => "Set Pages",
                            CameraKind::LineScan => "Set Blocks",
                            _ => "Set Frames",
                        };
                        step = step.push(link(
                            label,
                            "Set the frame count",
                            Some(Message::SetFrameCount),
                        ));
                        step = step.push(
                            text("then Capture, or capture on a schedule")
                                .size(style::SMALL)
                                .color(p.secondary),
                        );
                    }
                }
            } else if index == 2
                && done[index]
                && let Some(saved) = &self.last_saved
            {
                // Where the last capture went, once there is one.
                step = step.push(space().width(4)).push(link(
                    reveal::label(),
                    "Show the captured file",
                    Some(Message::Reveal(saved.path.clone().into())),
                ));
            }
            steps = steps.push(step.wrap().vertical_spacing(6));
        }
        steps.into()
    }

    /// Hand a message to the capture (`recording` false) or recording
    /// destination picker, focusing the first field of an editor it opens.
    pub(super) fn picker(
        &mut self,
        recording: bool,
        message: destinations::Message,
    ) -> Task<Message> {
        if matches!(
            message,
            destinations::Message::Undo | destinations::Message::Redo
        ) {
            let editor = if recording {
                self.record_to.editor_open()
            } else {
                self.capture_to.editor_open()
            };
            if !editor {
                return self.replay_history(matches!(message, destinations::Message::Undo));
            }
        }
        let (picker, output) = if recording {
            (&mut self.record_to, &mut self.forward_output)
        } else {
            (&mut self.capture_to, &mut self.output)
        };
        let was_open = picker.manager_open;
        picker.update(message, output);
        picker_page(picker, was_open)
    }

    /// Bring a destination sheet whose page changed on its own, as a
    /// finished save does, to its top; from `tick()`.
    /// Also opens Settings when the measurements asked for it, which needs
    /// a task this tick can carry (see `take_settings_request`).
    pub(super) fn pickers_to_top(&mut self) -> Task<Message> {
        Task::batch([
            page_top(&mut self.capture_to),
            page_top(&mut self.record_to),
            self.take_settings_request(),
            self.take_assistant_handoff(),
        ])
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
        // The parent remains at the same position in the widget tree as a
        // sheet opens and closes, preserving its field focus and selection.
        let body = keyboard_history::background(body, self.sheet_open());
        let t = self.sheets.shown.get(self.now);
        let (height, spin, now) = (self.sheet_height(), self.spin(), self.now);
        if let Some(sheet) = self.sign_in_sheet(p) {
            modal(
                body,
                sheet,
                Some(Message::SignIn(sign_in::Input::Cancel)),
                t,
            )
        } else if self.settings_open && self.settings_window.is_none() {
            modal(
                body,
                self.settings_sheet(p, height),
                Some(Message::Settings(false)),
                t,
            )
        } else if self.help_open && self.sheets.agents {
            modal(
                body,
                self.agent_setup(p, height),
                Some(Message::AgentSetup(false)),
                t,
            )
        } else if self.help_open {
            modal(body, self.help(p, height), Some(Message::Help(false)), t)
        } else if self.assistant.quick.open {
            modal(
                body,
                self.assistant_sheet(p, height),
                Some(Message::Assistant(assistant::Input::Quick(
                    assistant::quick::Input::Open(false),
                ))),
                t,
            )
        } else if self.network_open {
            modal(
                body,
                self.network_sheet(p, height),
                Some(Message::NetworkReport(false)),
                t,
            )
        } else if self.resources.open {
            modal(
                body,
                self.resources_sheet(p, height),
                Some(Message::Resources(resources::Input::Open(false))),
                t,
            )
        } else if self.updates.open {
            modal(
                body,
                self.updates_sheet(p, height),
                Some(Message::Updates(updates::Input::Close)),
                t,
            )
        } else if self.analytics.open {
            modal(
                body,
                self.analytics_sheet(p, height),
                Some(Message::Analytics(false)),
                t,
            )
        } else if self.capture_to.manager_open {
            modal(
                body,
                accessibility::mapped(
                    self.capture_to.manager(
                        p,
                        height,
                        spin,
                        now,
                        (!self.history_busy())
                            .then(|| self.history_label(true))
                            .flatten(),
                        (!self.history_busy())
                            .then(|| self.history_label(false))
                            .flatten(),
                    ),
                    Message::CapturePicker,
                ),
                (!self.capture_to.editing())
                    .then_some(Message::CapturePicker(destinations::Message::CloseManager)),
                t,
            )
        } else if self.record_to.manager_open {
            modal(
                body,
                accessibility::mapped(
                    self.record_to.manager(
                        p,
                        height,
                        spin,
                        now,
                        (!self.history_busy())
                            .then(|| self.history_label(true))
                            .flatten(),
                        (!self.history_busy())
                            .then(|| self.history_label(false))
                            .flatten(),
                    ),
                    Message::RecordPicker,
                ),
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
            stack![body].into()
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
        let [editing, cameras, acquisition, view, panels, window] = Action::SECTIONS;
        let more = self.sheets.more;
        let mut body = column![
            // Only while a newer version is available.
            self.updates_help_row(p),
            self.guide_steps_view(p),
            disclosure(
                "Connecting cameras",
                None,
                self.sheets.more_turn.get(self.now),
                Message::HelpMore(!more),
                p,
            ),
        ]
        .spacing(14);
        if more {
            body = body.push(connection_notes(self.just_copied(HUB_COMMAND), p));
        }
        let body = body
            .push(
                row![
                    column![section(editing), section(cameras), section(acquisition), section(window)]
                        .spacing(18)
                        .width(Fill),
                    column![section(view), section(panels)].spacing(18).width(Fill),
                ]
                .spacing(32),
            )
            .push(
                text("Undo and Redo work in text fields and restore reversible settings changes. Captured files stay saved.")
                    .size(style::SMALL)
                    .color(p.secondary),
            )
            .push(text(plain_keys_note(os)).size(style::SMALL).color(p.secondary))
            .push(heading("Control this window from a terminal or agent", p))
            .push(
                text("Copy the session command to attach your terminal or coding agent. Everyone shares camera connections, settings and captures. Activity shows who acted and capture progress; your selected camera and unfinished edits stay local.")
                    .size(style::BODY),
            )
            .push(self.copyable(self.session_command(), p))
            .push(self.copyable("capturefab --help".into(), p))
            .push(
                button(text("Connect an external agent…").size(style::BODY))
                    .name("Open agent setup")
                    .style(style::secondary)
                    .padding([7, 12])
                    .on_press(Message::AgentSetup(true)),
            )
            // For a bug report: Help > Report a Problem fills it in.
            .push(
                text(menus::version_line())
                    .size(style::CAPTION)
                    .color(p.secondary),
            );
        sheet_frame("Quick guide", Message::Help(false), body, p)
            .mark(Icon::Keyboard)
            .size(700.0, height)
            .scrolled(self.sheets.scrolled)
            .on_scroll(Message::HelpScrolled)
            .into()
    }
}

/// The guide's note on plain keys: they act on the workbench only while no
/// field has keyboard focus. Lists every one the guide shows, in its order.
fn plain_keys_note(os: Os) -> String {
    let keys: Vec<String> = Action::SECTIONS
        .iter()
        .flat_map(|(_, actions)| actions.iter())
        .filter(|action| {
            action
                .display_binding(os)
                .is_some_and(|chord| chord.needs_free_keyboard())
        })
        .map(|action| action.key_label(os))
        .collect();
    let keys = match keys.split_last() {
        Some((last, rest)) if !rest.is_empty() => format!("{} and {last}", rest.join(", ")),
        Some((last, _)) => last.clone(),
        None => String::new(),
    };
    format!(
        "{keys} act on the workbench when no field has keyboard focus. Press {} or click elsewhere to leave a field.",
        Action::Overview.key_label(os)
    )
}

/// A guide step's action: a small button, its shortcut beside the label.
fn step_button<'a>(
    label: &'static str,
    key: Option<String>,
    name: &'static str,
    on: Option<Message>,
    p: &'static Palette,
) -> Element<'a, Message> {
    let mut content = row![text(label).size(style::SMALL)]
        .spacing(6)
        .align_y(Alignment::Center);
    if let Some(key) = key.filter(|key| !key.is_empty()) {
        content = content.push(text(key).size(style::CAPTION).color(p.secondary));
    }
    button(content)
        .name(name)
        .style(style::secondary)
        .padding([3, 9])
        .on_press_maybe(on)
        .into()
}

/// The native camera scheme on this system, for the webcam note.
fn webcam_scheme() -> &'static str {
    if cfg!(target_os = "macos") {
        "avfoundation://"
    } else if cfg!(target_os = "windows") {
        "dshow://"
    } else {
        "v4l2://"
    }
}

/// How the guide's notes save a hub, as README.md shows it: the secret
/// comes from a file on standard input, never the command line.
const HUB_COMMAND: &str = "capturefab hub add-home-assistant home --url http://homeassistant.local:8123 --secret-stdin < token.txt";

/// The guide's notes on connecting cameras: one short line per kind of
/// source, what to type for it in small monospace. `hub_copied`: the hub
/// command was just copied.
fn connection_notes<'a>(hub_copied: bool, p: &'static Palette) -> Element<'a, Message> {
    let note = |kind: &'static str, about: &'static str, syntax: Option<&'static str>| {
        let mut detail = column![
            text(about)
                .size(style::SMALL)
                .color(p.secondary)
                .width(Fill)
                .wrapping(text::Wrapping::WordOrGlyph),
        ]
        .spacing(3)
        .width(Fill);
        if let Some(syntax) = syntax {
            detail = detail.push(
                text(syntax)
                    .size(style::CAPTION)
                    .font(style::MONO)
                    .color(p.secondary),
            );
        }
        row![
            text(kind)
                .size(style::SMALL)
                .font(style::MEDIUM)
                .color(p.text)
                .width(92),
            detail,
        ]
        .spacing(8)
        .align_y(Alignment::Start)
    };
    column![
        note(
            "Streams",
            "Paste an RTSP, SRT, RTMP or HTTP address.",
            Some("rtsp://host/stream")
        ),
        note(
            "Video files",
            "Loop a file, or a folder's frames by name (not subfolders).",
            Some("sim:replay:PATH")
        ),
        note(
            "Webcams",
            "Built-in and USB webcams appear with the other cameras once the system allows camera access.",
            Some(webcam_scheme())
        ),
        note(
            "Hubs",
            "Save a Home Assistant server once in a terminal; its cameras then appear with the others.",
            Some("hub:NAME/CAMERA")
        ),
        row![
            space().width(100),
            column![
                container(code_block(HUB_COMMAND.into(), hub_copied, p)).width(Fill),
                text(
                    "For Frigate, go2rtc or UniFi Protect, use add-frigate, add-go2rtc or add-unifi-protect (see capturefab hub --help)."
                )
                .size(style::SMALL)
                .color(p.secondary)
                .width(Fill)
                .wrapping(text::Wrapping::WordOrGlyph),
            ]
            .spacing(4)
            .width(Fill),
        ],
        note("GigE", "Use an address on the camera's subnet.", None).push(tip(
            step_button("Check network", None, "Check network", Some(Message::ScanNetwork), p),
            "Tunes connected GigE cameras for the available bandwidth and checks network adapters. Live previews may pause briefly.",
        )),
        note(
            "USB3",
            "The operating system must allow access to the camera.",
            None
        ),
        text("Activity shows connection and capture errors.")
            .size(style::SMALL)
            .color(p.secondary),
    ]
    .spacing(6)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quick_guide_first_step_actions_keep_readable_button_labels_in_compact_windows() {
        use crate::gui::destinations::layout_tests::renderer;
        use iced::Rectangle;
        use iced::advanced::{Layout, layout, widget::Operation, widget::Tree};

        #[derive(Default)]
        struct Actions(Vec<(String, Rectangle)>);
        impl Operation for Actions {
            fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
                operate(self);
            }
            fn custom(
                &mut self,
                _: Option<&iced::advanced::widget::Id>,
                bounds: Rectangle,
                state: &mut dyn std::any::Any,
            ) {
                if let Some(control) = state.downcast_ref::<accessibility::Control<Message>>()
                    && control.node.role() == accessibility::Role::Button
                {
                    self.0.push((control.node.label().unwrap().into(), bounds));
                }
            }
        }
        let renderer = renderer();
        // The first step's offers, then the last step's with the file link.
        let mut later = bench();
        later.snapshot.cameras = vec![super::super::liveness::streaming_camera(10, 0, 30.0)];
        later.last_saved =
            Saved::from_result(None, &serde_json::json!({ "files": ["f.png"] }), later.now);
        for app in [bench(), later] {
            let mut baseline = vec![];
            for (index, width) in [1000.0, 480.0, 480.0 / UiScale::Larger.factor()]
                .into_iter()
                .enumerate()
            {
                let mut view = app.guide_steps_view(Palette::of(false));
                let mut tree = Tree::new(view.as_widget());
                let node = view.as_widget_mut().layout(
                    &mut tree,
                    &renderer,
                    &layout::Limits::new(Size::ZERO, Size::new(width, 1000.0)),
                );
                let mut actions = Actions::default();
                view.as_widget_mut().operate(
                    &mut tree,
                    Layout::new(&node),
                    &renderer,
                    &mut actions,
                );
                assert_eq!(actions.0.len(), 2);
                if index == 0 {
                    baseline = actions.0;
                    continue;
                }
                for (name, bounds) in actions.0 {
                    let natural = baseline.iter().find(|(label, _)| label == &name).unwrap().1;
                    assert!(
                        bounds.width + 0.5 >= natural.width
                            && bounds.height <= natural.height + 0.5,
                        "{name} keeps its label at {width} logical pixels: {bounds:?}; natural {natural:?}"
                    );
                    assert!(bounds.x >= 0.0 && bounds.x + bounds.width <= width + 0.5);
                }
            }
        }
    }

    #[test]
    fn connecting_camera_notes_keep_addresses_readable_at_every_interface_size() {
        use crate::gui::destinations::layout_tests::{render_artifact_themed, renderer};
        use iced::Rectangle;
        use iced::advanced::{Layout, layout, widget::Operation, widget::Tree};

        #[derive(Default)]
        struct NotesGeometry(Vec<(String, Rectangle)>);
        impl Operation for NotesGeometry {
            fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
                operate(self);
            }
            fn text(
                &mut self,
                _: Option<&iced::advanced::widget::Id>,
                bounds: Rectangle,
                value: &str,
            ) {
                self.0.push((value.into(), bounds));
            }
        }

        let mut renderer = renderer();
        let mut baseline = connection_notes(false, Palette::of(false));
        let mut baseline_tree = Tree::new(baseline.as_widget());
        let baseline_node = baseline.as_widget_mut().layout(
            &mut baseline_tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(1600.0, 1200.0)),
        );
        let mut baseline_geometry = NotesGeometry::default();
        baseline.as_widget_mut().operate(
            &mut baseline_tree,
            Layout::new(&baseline_node),
            &renderer,
            &mut baseline_geometry,
        );
        for (dark, copied) in [(false, false), (true, false), (false, true), (true, true)] {
            for scale in UiScale::ALL {
                let viewport = Size::new(480.0, 1200.0) / scale.factor();
                let mut view = connection_notes(copied, Palette::of(dark));
                let mut tree = Tree::new(view.as_widget());
                let node = view.as_widget_mut().layout(
                    &mut tree,
                    &renderer,
                    &layout::Limits::new(Size::ZERO, viewport),
                );
                let mut geometry = NotesGeometry::default();
                view.as_widget_mut().operate(
                    &mut tree,
                    Layout::new(&node),
                    &renderer,
                    &mut geometry,
                );
                for syntax in [
                    "rtsp://host/stream",
                    "sim:replay:PATH",
                    webcam_scheme(),
                    "hub:NAME/CAMERA",
                ] {
                    let bounds = geometry
                        .0
                        .iter()
                        .find(|(value, _)| value == syntax)
                        .unwrap()
                        .1;
                    let natural = baseline_geometry
                        .0
                        .iter()
                        .find(|(value, _)| value == syntax)
                        .unwrap()
                        .1;
                    assert!(
                        bounds.width + 0.5 >= natural.width && bounds.height <= 24.0,
                        "{syntax} must stay readable at {scale}: {bounds:?}"
                    );
                    assert!(bounds.x + bounds.width <= viewport.width + 0.5);
                }
                render_artifact_themed(
                    &format!(
                        "connection-notes-{}{}-{scale}",
                        if dark { "dark" } else { "light" },
                        if copied { "-copied" } else { "" }
                    ),
                    &view,
                    &tree,
                    &node,
                    &mut renderer,
                    viewport,
                    (dark, scale.factor()),
                );
            }
        }
    }

    /// The hub command the guide offers to copy reads its secret from a
    /// file, so no credential is ever on the clipboard or in shell history.
    #[test]
    fn the_hub_command_carries_no_secret() {
        assert!(HUB_COMMAND.contains("--secret-stdin < token.txt"));
        let url = HUB_COMMAND
            .split_whitespace()
            .skip_while(|word| *word != "--url")
            .nth(1)
            .unwrap();
        assert!(!url.contains('@') && !url.contains("token="), "{url}");
        assert!(
            include_str!("../../README.md").contains(HUB_COMMAND),
            "matches the README"
        );
    }

    fn bench() -> Workbench {
        Workbench::new(SessionHandle::new(), "test".into(), true, None)
    }

    fn press(bench: &mut Workbench, action: Action) {
        let chord = action.bindings(Os::CURRENT)[0];
        let _ = bench.shortcut(chord, true);
    }

    #[test]
    fn destination_pages_focus_opened_sheet_and_restore_list_after_editor_cancel() {
        let mut bench = bench();
        let picker = &mut bench.capture_to;
        let mut output = "capture.png".to_owned();
        picker.update(destinations::Message::OpenManager, &mut output);
        assert_eq!(picker_focus(picker, false, false), Some("sheet-close"));
        picker.update(destinations::Message::AddFolder, &mut output);
        let changed = picker.take_to_top();
        assert!(changed);
        let field = picker_focus(picker, true, changed).expect("editor focus");
        assert_ne!(
            field, "sheet-close",
            "editor's first field takes precedence"
        );
        picker.update(destinations::Message::Cancel, &mut output);
        let changed = picker.take_to_top();
        assert_eq!(picker_focus(picker, true, changed), Some("sheet-close"));
        assert_eq!(
            picker_focus(picker, true, false),
            None,
            "ordinary ticks do not reset focus"
        );
        picker.update(destinations::Message::CloseManager, &mut output);
        let changed = picker.take_to_top();
        assert_eq!(
            picker_focus(picker, true, changed),
            None,
            "closed sheets retain parent focus"
        );
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
    fn connection_tips_open_the_guide_at_its_notes() {
        let mut bench = bench();
        let _ = bench.handle_message(Message::HelpMore(true));
        assert!(
            bench.help_open && bench.sheets.more,
            "from the welcome screen"
        );
        assert_eq!(bench.sheets.more_turn.get(bench.now), 1.0, "already open");
        let _ = bench.handle_message(Message::HelpMore(false));
        assert!(
            bench.help_open && !bench.sheets.more,
            "the fold closes them"
        );
        // Another sheet keeps the guide waiting.
        let mut bench = self::bench();
        bench.capture_to.manager_open = true;
        let _ = bench.handle_message(Message::HelpMore(true));
        assert!(!bench.help_open && !bench.sheets.more);
    }

    #[test]
    fn the_guide_marks_its_first_steps_as_they_are_done() {
        let mut bench = bench();
        assert_eq!(bench.guide_steps(), [false; 4]);
        let mut camera = super::super::liveness::streaming_camera(10, 0, 30.0);
        camera.streaming = false;
        bench.snapshot.cameras = vec![camera];
        assert_eq!(bench.guide_steps(), [true, false, false, false]);
        bench.snapshot.cameras[0].streaming = true;
        assert_eq!(bench.guide_steps(), [true, true, false, false]);
        bench.last_saved = Saved::from_result(
            None,
            &serde_json::json!({ "files": ["frame-1.png"] }),
            bench.now,
        );
        assert_eq!(bench.guide_steps(), [true, true, true, false]);
        // Two files from one capture, as `finished` records them.
        let pending = Pending::unanswered(Job::Capture);
        bench.finished(
            &pending,
            &serde_json::json!({ "files": ["frame-2.png", "frame-3.png"] }),
        );
        assert_eq!(bench.guide_steps(), [true; 4]);
        // The saved line goes; the series stays done.
        bench.last_saved = None;
        assert_eq!(bench.guide_steps(), [true, true, false, true]);
    }

    /// The guide's controls by name and what each sends, at the sheet's
    /// size; the steps and notes come first.
    fn guide_buttons(bench: &Workbench) -> Vec<(String, Option<Message>)> {
        use iced::advanced::{Layout, layout, widget::Tree};
        use iced::futures::StreamExt;
        let renderer = super::super::destinations::layout_tests::renderer();
        let mut view = bench.help(Palette::of(false), 820.0);
        let mut tree = Tree::new(view.as_widget());
        let node = view.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(1000.0, 900.0)),
        );
        let (snapshot, bridge, window) = accessibility::snapshot_widget(
            view.as_widget_mut(),
            &mut tree,
            Layout::new(&node),
            &renderer,
        );
        snapshot
            .nodes
            .iter()
            .filter(|(_, node)| node.role() == accessibility::Role::Button)
            .map(|(id, node)| {
                let task = bridge.dispatch(accessibility::Request {
                    window,
                    replay: None,
                    action: accesskit::ActionRequest {
                        action: accesskit::Action::Click,
                        target: *id,
                        data: None,
                    },
                });
                let sent = iced_runtime::task::into_stream(task).and_then(|mut actions| {
                    match iced::futures::executor::block_on(actions.next()) {
                        Some(iced_runtime::Action::Output(message)) => Some(message),
                        _ => None,
                    }
                });
                (node.label().unwrap_or_default().to_owned(), sent)
            })
            .collect()
    }

    #[test]
    fn the_next_step_offers_what_does_it_under_a_name() {
        let mut bench = bench();
        bench.help_open = true;
        let sent = |buttons: &[(String, Option<Message>)], name: &str| {
            buttons
                .iter()
                .find(|(label, _)| label == name)
                .unwrap_or_else(|| panic!("{name} in the guide"))
                .1
                .clone()
        };
        let buttons = guide_buttons(&bench);
        assert!(
            buttons.iter().all(|(label, _)| !label.is_empty()),
            "{buttons:?}"
        );
        // The steps lead; connecting an agent waits under Automation.
        let at = |name: &str| buttons.iter().position(|(label, _)| label == name);
        assert!(
            at("Enter an address") < at("Open agent setup"),
            "{buttons:?}"
        );
        assert!(matches!(
            sent(&buttons, "Open agent setup"),
            Some(Message::AgentSetup(true))
        ));
        assert!(matches!(
            sent(&buttons, "Enter an address"),
            Some(Message::EnterAddress)
        ));
        assert!(matches!(
            sent(&buttons, "Try the simulator"),
            Some(Message::TrySimulator)
        ));
        assert!(!buttons.iter().any(|(label, _)| label == "Start the stream"));
        // A simulated camera already listed: no offer to try one.
        bench.snapshot.devices = vec![crate::transport::simulator::info()];
        let buttons = guide_buttons(&bench);
        assert!(
            !buttons
                .iter()
                .any(|(label, _)| label == "Try the simulator")
        );
        assert!(matches!(
            sent(&buttons, "Enter an address"),
            Some(Message::EnterAddress)
        ));
        bench.snapshot.devices.clear();
        // Connected: the next step's link, and the first step's are gone.
        let mut camera = super::super::liveness::streaming_camera(10, 0, 30.0);
        camera.streaming = false;
        bench.snapshot.active_camera = Some(camera.info.id.clone());
        bench.snapshot.cameras = vec![camera];
        let buttons = guide_buttons(&bench);
        assert!(matches!(
            sent(&buttons, "Start the stream"),
            Some(Message::ToggleStream)
        ));
        assert!(!buttons.iter().any(|(label, _)| label == "Enter an address"));
        bench.snapshot.cameras[0].streaming = true;
        let buttons = guide_buttons(&bench);
        assert!(
            matches!(sent(&buttons, "Capture a frame"), Some(Message::Capture)),
            "the streaming camera can capture: {:?}",
            sent(&buttons, "Capture a frame")
        );
        // One frame captured: the next step sets the frame count, and the
        // captured file is a click away.
        bench.last_saved =
            Saved::from_result(None, &serde_json::json!({ "files": ["f.png"] }), bench.now);
        let buttons = guide_buttons(&bench);
        assert!(matches!(
            sent(&buttons, "Set the frame count"),
            Some(Message::SetFrameCount)
        ));
        assert!(matches!(
            sent(&buttons, "Show the captured file"),
            Some(Message::Reveal(path)) if path == std::path::Path::new("f.png")
        ));
        assert!(!buttons.iter().any(|(label, _)| label == "Capture a frame"));
        // The notes hold the network check.
        bench.sheets.more = true;
        let buttons = guide_buttons(&bench);
        assert!(matches!(
            sent(&buttons, "Check network"),
            Some(Message::ScanNetwork)
        ));
    }

    #[test]
    fn the_guide_makes_way_for_what_its_links_open() {
        let mut bench = bench();
        let _ = bench.handle_message(Message::Help(true));
        let _ = bench.handle_message(Message::EnterAddress);
        assert!(!bench.help_open, "the address field is under the guide");
        let _ = bench.handle_message(Message::Help(true));
        bench.sheets.more = true;
        let _ = bench.handle_message(Message::ScanNetwork);
        assert!(
            !bench.help_open && bench.network_open,
            "the report takes its place"
        );
        let _ = bench.handle_message(Message::NetworkReport(false));
        let _ = bench.handle_message(Message::Help(true));
        bench.tab = Tab::Forward;
        let _ = bench.handle_message(Message::SetFrameCount);
        assert!(
            !bench.help_open && bench.tab == Tab::Capture && bench.sidebar_open,
            "the Capture panel's frame count is under the guide"
        );
    }

    #[test]
    fn esc_leaves_a_sheet_even_from_the_field_it_focused() {
        let mut bench = bench();
        let esc = Action::Overview.bindings(Os::CURRENT)[0];
        bench.capture_to.manager_open = true;
        let _ = bench.handle_message(Message::CapturePicker(destinations::Message::AddFolder));
        assert!(bench.capture_to.editing());
        // The focused field took the key to leave itself.
        let _ = bench.shortcut(esc, false);
        assert!(!bench.capture_to.editing(), "back to the list");
        assert!(bench.capture_to.manager_open);
        let _ = bench.shortcut(esc, false);
        assert!(!bench.capture_to.manager_open, "closed");
        // Outside a sheet, Esc in a field only leaves the field.
        bench.image_mode = true;
        let _ = bench.shortcut(esc, false);
        assert!(bench.image_mode);
    }

    #[test]
    fn the_notes_chevron_turns_and_settles() {
        let _motion = motion::test_reduce_motion(false);
        let mut bench = bench();
        let _ = bench.handle_message(Message::Help(true));
        bench.sync_animations();
        let start = bench.now;
        let _ = bench.handle_message(Message::HelpMore(true));
        bench.sync_animations();
        let mid = bench.sheets.more_turn.get(start + motion::RING / 2);
        assert!(mid > 0.0 && mid < 1.0, "turns: {mid}");
        bench.now = start + motion::RING;
        assert_eq!(bench.sheets.more_turn.get(bench.now), 1.0);
        assert!(!bench.sheets.more_turn.animating(bench.now));
        // Reopened, the guide shows its notes closed without turning them.
        let _ = bench.handle_message(Message::Help(false));
        let _ = bench.handle_message(Message::Help(true));
        bench.sync_animations();
        assert_eq!(bench.sheets.more_turn.get(bench.now), 0.0);
        assert!(!bench.sheets.more_turn.animating(bench.now));
    }

    #[test]
    fn the_guide_names_every_key_a_field_takes() {
        assert_eq!(
            plain_keys_note(Os::Mac),
            "Esc, Space, F, H and R act on the workbench when no field has keyboard focus. Press Esc or click elsewhere to leave a field."
        );
        // F1 and F11 work from a field, so they are not named.
        assert!(plain_keys_note(Os::Windows).starts_with("Esc, Space, F, H and R act"));
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
