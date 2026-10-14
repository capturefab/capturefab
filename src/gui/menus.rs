//! The command menus share the workbench's actions and availability. AppKit
//! owns the macOS menu bar, including keyboard navigation and system commands.
use super::*;
mod desktop;
pub(super) use desktop::{DesktopState, Input as DesktopInput};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Command {
    Action(Action),
    Edit(keyboard_history::EditCommand),
    Disconnect,
    AllStreams(bool),
    AllAuto(bool),
    RefreshFeatures,
    Destinations,
    Record,
    Schedule,
    Quit,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Item {
    pub title: String,
    pub command: Command,
    pub enabled: bool,
    pub checked: bool,
    pub shortcut: Option<Chord>,
}

impl Item {
    fn action(title: impl Into<String>, action: Action, enabled: bool) -> Self {
        Self {
            title: title.into(),
            command: Command::Action(action),
            enabled,
            checked: false,
            // A plain letter, Space, Return or Escape belongs to a focused
            // control. AppKit key equivalents would intercept it globally.
            shortcut: action
                .bindings(Os::CURRENT)
                .into_iter()
                .find(|key| !key.needs_free_keyboard() && action != Action::Overview),
        }
    }

    fn command(title: impl Into<String>, command: Command, enabled: bool) -> Self {
        Self {
            title: title.into(),
            command,
            enabled,
            checked: false,
            shortcut: None,
        }
    }

    fn checked(mut self, checked: bool) -> Self {
        self.checked = checked;
        self
    }
}

/// `None` is a system separator, never an inert command.
pub(super) struct Menu {
    pub title: &'static str,
    pub items: Vec<Option<Item>>,
}

impl Workbench {
    pub(super) fn command_menus(&self) -> Vec<Menu> {
        let ready = !self.sheet_open()
            && self.settings_window != Some(self.focused_window)
            && self.closing.is_none();
        let connected = self.snapshot.connected.is_some();
        let camera = connected && ready;
        let scanner = self.scanner_selected();
        let streaming = self.snapshot.streaming;
        let image = ready && self.shown.is_some();
        let multiple = ready && self.snapshot.cameras.len() > 1;
        let auto = self.snapshot.auto.is_some();
        let busy = self.history_busy();
        let history = |undo| {
            let name = if undo { "Undo" } else { "Redo" };
            let label = self.history_label(undo);
            Some(Item::action(
                label.map_or_else(|| name.into(), |label| format!("{name} {label}")),
                if undo { Action::Undo } else { Action::Redo },
                label.is_some() && !busy && !self.help_open,
            ))
        };
        let edit = |title, command, key| {
            Some(Item {
                shortcut: Some(Chord {
                    key: keys::Key::Char(key),
                    mods: keys::Mods {
                        command: true,
                        ..keys::Mods::NONE
                    },
                }),
                ..Item::command(title, Command::Edit(command), self.menu_editor)
            })
        };
        let action = |title: &str, action, enabled| Some(Item::action(title, action, enabled));
        let mut menus = vec![
            Menu {
                title: "File",
                items: vec![
                    action("Connect to Camera…", Action::ConnectAddress, ready),
                    action(
                        "Discover Cameras",
                        Action::Discover,
                        ready && !self.pending(Job::Discover),
                    ),
                    None,
                    action(
                        "Capture and Save",
                        Action::Capture,
                        camera && !self.pending(Job::Capture),
                    ),
                    Some(Item::command(
                        "Schedule Capture",
                        Command::Schedule,
                        camera && !self.pending(Job::Schedule),
                    )),
                    Some(Item::command(
                        if self.snapshot.forwarding.is_some() {
                            "Stop Recording or Forwarding"
                        } else {
                            "Set Up Recording or Forwarding…"
                        },
                        Command::Record,
                        camera && !scanner && self.forward_pending().is_none(),
                    )),
                    Some(Item::command(
                        "Manage Destinations…",
                        Command::Destinations,
                        ready,
                    )),
                    None,
                    action("Close Window", Action::CloseWindow, true),
                ],
            },
            Menu {
                title: "Edit",
                items: vec![
                    history(true),
                    history(false),
                    None,
                    edit("Cut", keyboard_history::EditCommand::Cut, 'x'),
                    edit("Copy", keyboard_history::EditCommand::Copy, 'c'),
                    edit("Paste", keyboard_history::EditCommand::Paste, 'v'),
                    edit("Select All", keyboard_history::EditCommand::SelectAll, 'a'),
                    None,
                    action("Find Camera Feature…", Action::SearchFeatures, camera),
                    action(
                        "Copy Session Command",
                        Action::CopySessionCommand,
                        self.settings_window != Some(self.focused_window),
                    ),
                ],
            },
            Menu {
                title: "View",
                items: vec![
                    Some(
                        Item::action("Show Camera List", Action::ToggleSidebar, ready)
                            .checked(self.sidebar_open && !self.image_mode),
                    ),
                    Some(
                        Item::action("Show Inspector", Action::ToggleInspector, ready).checked(
                            (self.inspector_open || self.inspector_peek) && !self.image_mode,
                        ),
                    ),
                    Some(
                        Item::action("Show Activity", Action::ToggleActivity, ready)
                            .checked(self.logs_open),
                    ),
                    None,
                    Some(
                        Item::action("Features", Action::FeaturesTab, camera)
                            .checked(self.tab == Tab::Features),
                    ),
                    Some(
                        Item::action("Capture", Action::CaptureTab, ready)
                            .checked(self.tab == Tab::Capture),
                    ),
                    Some(
                        Item::action("Record and Forward", Action::ForwardTab, camera && !scanner)
                            .checked(self.tab == Tab::Forward),
                    ),
                    None,
                    action("Zoom In", Action::ZoomIn, image),
                    action("Zoom Out", Action::ZoomOut, image),
                    Some(Item::action("Zoom to Fit", Action::ZoomFit, image).checked(self.fit)),
                    Some(
                        Item::action("Actual Pixels", Action::ZoomActual, image)
                            .checked(!self.fit && self.zoom == 1.0),
                    ),
                    None,
                    Some(
                        Item::action(
                            "Show Histogram",
                            Action::ToggleExposure,
                            ready && self.intensity_image(),
                        )
                        .checked(self.exposure_open),
                    ),
                    Some(
                        Item::action(
                            "Show Focus Region",
                            Action::ToggleFocusRegion,
                            ready
                                && self.intensity_image()
                                && self.camera_kind() != CameraKind::LineScan,
                        )
                        .checked(self.focus.open),
                    ),
                    Some(
                        Item::action("Image Only", Action::ImageMode, ready)
                            .checked(self.image_mode),
                    ),
                    action(
                        if self.fullscreen {
                            "Exit Full Screen"
                        } else {
                            "Enter Full Screen"
                        },
                        Action::Fullscreen,
                        self.settings_window != Some(self.focused_window),
                    ),
                ],
            },
            Menu {
                title: "Camera",
                items: vec![
                    action("Next Camera", Action::NextCamera, multiple),
                    action("Previous Camera", Action::PreviousCamera, multiple),
                    action(
                        "Open Selected Camera",
                        Action::FocusCamera,
                        multiple && self.overview(),
                    ),
                    action(
                        "Show All Cameras",
                        Action::Overview,
                        multiple && !self.overview(),
                    ),
                    None,
                    action(
                        if streaming {
                            "Stop Stream"
                        } else {
                            "Start Stream"
                        },
                        Action::ToggleStream,
                        camera
                            && !scanner
                            && self
                                .snapshot
                                .active_camera
                                .as_deref()
                                .is_none_or(|id| self.stream_pending(id).is_none()),
                    ),
                    action(
                        if auto {
                            "Use Manual Exposure"
                        } else {
                            "Use Auto Exposure"
                        },
                        Action::ToggleAuto,
                        camera
                            && (auto || self.camera_kind().supports_auto())
                            && !self.auto_busy_here(),
                    ),
                    Some(Item::command(
                        "Refresh Features",
                        Command::RefreshFeatures,
                        camera && !self.pending(Job::RefreshFeatures),
                    )),
                    None,
                    Some(Item::command(
                        "Start All Streams",
                        Command::AllStreams(true),
                        multiple
                            && self.snapshot.cameras.iter().any(|camera| {
                                !camera.streaming
                                    && camera.info.profile.kind != CameraKind::DocumentScanner
                            }),
                    )),
                    Some(Item::command(
                        "Stop All Streams",
                        Command::AllStreams(false),
                        multiple && self.snapshot.cameras.iter().any(|camera| camera.streaming),
                    )),
                    Some(Item::command(
                        "Use Auto Exposure for All",
                        Command::AllAuto(true),
                        multiple
                            && self.snapshot.cameras.iter().any(|camera| {
                                camera.auto.is_none() && camera.info.profile.kind.supports_auto()
                            }),
                    )),
                    Some(Item::command(
                        "Use Manual Exposure for All",
                        Command::AllAuto(false),
                        multiple
                            && self
                                .snapshot
                                .cameras
                                .iter()
                                .any(|camera| camera.auto.is_some()),
                    )),
                    None,
                    Some(Item::command(
                        "Disconnect Camera",
                        Command::Disconnect,
                        camera && !self.pending(Job::Disconnect),
                    )),
                ],
            },
            Menu {
                title: "Help",
                items: vec![action(
                    "Capturefab Help",
                    Action::Help,
                    ready || self.help_open,
                )],
            },
        ];
        if !cfg!(target_os = "macos") {
            menus[0].items.push(None);
            menus[0].items.push(Some(Item {
                shortcut: Action::Quit.display_binding(Os::CURRENT),
                ..Item::command(
                    if cfg!(target_os = "windows") {
                        "Exit"
                    } else {
                        "Quit"
                    },
                    Command::Quit,
                    true,
                )
            }));
        }
        if self.close_error.is_some() {
            for item in menus
                .iter_mut()
                .flat_map(|menu| menu.items.iter_mut().flatten())
            {
                item.enabled = matches!(
                    item.command,
                    Command::Quit | Command::Action(Action::CloseWindow)
                );
            }
        }
        menus
    }

    pub(super) fn menu_command(&mut self, command: Command) -> Task<Message> {
        if self.close_error.is_some() && !matches!(command, Command::Quit | Command::Action(_)) {
            return Task::none();
        }
        match command {
            Command::Quit => self.handle_message(Message::Quit),
            Command::Action(action) => self.dispatch_action(action),
            Command::Edit(command) => {
                let serial = self
                    .menu_edit
                    .map_or(1, |request| request.serial.wrapping_add(1));
                self.menu_edit = Some(keyboard_history::EditRequest { serial, command });
                Task::none()
            }
            _ if self.sheet_open() || self.settings_window == Some(self.focused_window) => {
                Task::none()
            }
            Command::Disconnect => {
                if let Some(id) = self.snapshot.active_camera.clone() {
                    return self.handle_message(Message::Disconnect(id));
                }
                Task::none()
            }
            Command::AllStreams(value) => self.handle_message(Message::AllStreams(value)),
            Command::AllAuto(value) => self.handle_message(Message::AllAuto(value)),
            Command::RefreshFeatures => self.handle_message(Message::RefreshFeatures),
            Command::Destinations => {
                self.handle_message(Message::CapturePicker(destinations::Message::OpenManager))
            }
            Command::Record => {
                if self.snapshot.forwarding.is_some() {
                    self.handle_message(Message::StopForward)
                } else {
                    self.handle_message(Message::ShowTab(Tab::Forward))
                }
            }
            Command::Schedule => {
                self.schedule_enabled = true;
                self.handle_message(Message::ShowTab(Tab::Capture))
            }
        }
    }
}

#[cfg(target_os = "macos")]
mod native;
#[cfg(target_os = "macos")]
pub(super) use native::{subscription, sync};

#[cfg(not(target_os = "macos"))]
pub(super) fn sync(_app: &Workbench) {}

#[cfg(not(target_os = "macos"))]
pub(super) fn subscription() -> Subscription<Message> {
    Subscription::none()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bench() -> Workbench {
        Workbench::new(SessionHandle::new(), "menu-test".into(), false, None)
    }

    #[test]
    fn every_keyboard_action_is_discoverable_in_a_menu() {
        let app = bench();
        let listed: Vec<_> = app
            .command_menus()
            .into_iter()
            .flat_map(|menu| menu.items)
            .flatten()
            .filter_map(|item| match item.command {
                Command::Action(action) => Some(action),
                _ => None,
            })
            .collect();
        for action in Action::ALL {
            assert!(
                listed.contains(&action) || matches!(action, Action::Settings | Action::Quit),
                "{action:?} is missing from menus"
            );
        }
    }

    #[test]
    fn camera_commands_dim_without_a_camera_and_edits_follow_focus() {
        let mut app = bench();
        for menu in app.command_menus() {
            for item in menu.items.into_iter().flatten() {
                if matches!(
                    item.command,
                    Command::Action(Action::Capture | Action::ToggleStream | Action::ToggleAuto)
                        | Command::Disconnect
                        | Command::Edit(_)
                ) {
                    assert!(!item.enabled, "{}", item.title);
                }
            }
        }
        app.menu_editor = true;
        let editing: Vec<_> = app
            .command_menus()
            .into_iter()
            .flat_map(|menu| menu.items)
            .flatten()
            .filter(|item| matches!(item.command, Command::Edit(_)))
            .collect();
        assert_eq!(editing.len(), 4);
        assert!(editing.iter().all(|item| item.enabled));
    }

    #[test]
    fn menu_availability_matches_focused_settings_and_help_history() {
        let mut app = bench();
        let _ = app.update(Message::ToggleSidebar);
        let enabled = |app: &Workbench, wanted| {
            app.command_menus()
                .into_iter()
                .flat_map(|menu| menu.items)
                .flatten()
                .find(|item| item.command == Command::Action(wanted))
                .unwrap()
                .enabled
        };
        assert!(enabled(&app, Action::Undo));
        app.help_open = true;
        assert!(!enabled(&app, Action::Undo));
        app.help_open = false;
        let settings = window::Id::unique();
        app.settings_window = Some(settings);
        app.focused_window = settings;
        assert!(!enabled(&app, Action::Fullscreen));
        assert!(!enabled(&app, Action::CopySessionCommand));
        app.focused_window = app.main_window;
        assert!(enabled(&app, Action::Fullscreen));
        assert!(enabled(&app, Action::CopySessionCommand));
    }

    #[test]
    fn menu_equivalents_never_steal_typing_or_control_activation() {
        let app = bench();
        for item in app
            .command_menus()
            .into_iter()
            .flat_map(|menu| menu.items)
            .flatten()
        {
            assert!(
                item.shortcut.is_none_or(|key| !key.needs_free_keyboard()),
                "{}",
                item.title
            );
        }
    }
}
