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
    /// Starts or stops recording on every camera; which one is decided
    /// when it runs, because AppKit keeps each slot's command fixed.
    AllRecord,
    RefreshFeatures,
    Destinations,
    Record,
    Schedule,
    Quit,
    AgentSetup,
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
        let (recordings, recorders) = titlebar::record_counts(&self.snapshot.cameras);
        let batch = |job: Job| self.pending.iter().any(|p| p.batch && p.job == job);
        let history = |undo| {
            let name = if undo { "Undo" } else { "Redo" };
            let label = self.history_label(undo);
            Some(Item::action(
                label.map_or_else(|| name.into(), |label| format!("{name} {label}")),
                if undo { Action::Undo } else { Action::Redo },
                label.is_some()
                    && !busy
                    && !self.help_open
                    && !self.network_open
                    && !self.assistant.quick.open
                    && !self.analytics.open
                    && !self.resources.open,
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
                    action("Connect to Camera…", Action::NewWindow, ready),
                    action(
                        "Enter Camera Address",
                        Action::ConnectAddress,
                        ready && !camera,
                    ),
                    action(
                        "Quick Command…",
                        Action::QuickCommand,
                        ready || self.assistant.quick.open,
                    ),
                    action(
                        "Discover Cameras",
                        Action::Discover,
                        ready && !self.discovering_aloud(),
                    ),
                    None,
                    action(
                        "Capture and Save",
                        Action::Capture,
                        camera && !self.capturing_here(),
                    ),
                    action(
                        if self.recording_here() {
                            "Stop Recording"
                        } else {
                            "Start Recording"
                        },
                        Action::Record,
                        camera && !scanner && self.forward_pending().is_none(),
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
                        Item::action("Show Sidebar", Action::ToggleSidebar, camera)
                            .checked(self.sidebar_shown()),
                    ),
                    Some(
                        Item::action("Show Inspector", Action::ToggleInspector, camera)
                            .checked(self.inspector_shown()),
                    ),
                    Some(
                        Item::action("Show Activity", Action::ToggleActivity, ready)
                            .checked(self.logs_open),
                    ),
                    None,
                    action("Camera Features", Action::FeaturesTab, camera),
                    Some(
                        Item::action("Capture", Action::CaptureTab, camera)
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
                    action(
                        "Capture All",
                        Action::CaptureAll,
                        camera && !batch(Job::Capture),
                    ),
                    Some(Item::command(
                        if recordings > 0 {
                            "Stop All Recordings"
                        } else {
                            "Record All Cameras"
                        },
                        Command::AllRecord,
                        multiple
                            && (recordings > 0 || recorders > 0)
                            && !batch(Job::Forward { recording: true })
                            && !batch(Job::StopForward { recording: true }),
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
                items: vec![
                    Some(Item::command(
                        "Connect an Agent…",
                        Command::AgentSetup,
                        ready,
                    )),
                    action("Capturefab Help", Action::Help, ready || self.help_open),
                ],
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
            Command::AgentSetup => self.handle_message(Message::AgentSetup(true)),
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
            Command::AllRecord => {
                let (recordings, _) = titlebar::record_counts(&self.snapshot.cameras);
                self.handle_message(Message::AllRecord(recordings == 0))
            }
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
                self.reveal_schedule();
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
                listed.contains(&action)
                    || matches!(action, Action::Settings | Action::Quit | Action::Overview),
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
        let _ = app.update(Message::ToggleActivity);
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

    fn item(app: &Workbench, wanted: impl Fn(&Command) -> bool) -> Item {
        app.command_menus()
            .into_iter()
            .flat_map(|menu| menu.items)
            .flatten()
            .find(|item| wanted(&item.command))
            .unwrap()
    }

    #[test]
    fn every_batch_action_is_in_the_camera_menu() {
        let mut app = bench();
        let camera = liveness::streaming_camera(0, 0, 0.0);
        app.snapshot.connected = Some(camera.info.clone());
        app.snapshot.active_camera = Some(camera.info.id.clone());
        app.snapshot.cameras = vec![camera.clone()];
        let capture = |app: &Workbench| item(app, |c| *c == Command::Action(Action::CaptureAll));
        let record = |app: &Workbench| item(app, |c| *c == Command::AllRecord);
        // One camera: its own Record does it; Capture All is always there.
        assert!(capture(&app).enabled);
        assert!(!record(&app).enabled);
        let mut other = camera.clone();
        other.info.id = "sim:1".into();
        app.snapshot.cameras.push(other);
        let all = capture(&app);
        assert_eq!(all.title, "Capture All");
        assert!(all.enabled);
        let all = record(&app);
        assert_eq!(
            (all.title.as_str(), all.command),
            ("Record All Cameras", Command::AllRecord)
        );
        assert!(all.enabled);
        // Any recording turns it into the way to stop them all.
        app.snapshot.cameras[1].forwarding = Some("/tmp/sim-1.mp4".into());
        let all = record(&app);
        assert_eq!(
            (all.title.as_str(), all.command),
            ("Stop All Recordings", Command::AllRecord)
        );
        assert!(all.enabled);
        // A batch capture on its way holds the menu's.
        app.send_batch(vec!["sim:0".into()], Job::Capture, |_| SessionCommand::Stop);
        assert!(!capture(&app).enabled);
    }

    #[test]
    fn menu_commands_stay_put_while_recordings_start_and_stop() {
        // The native menu binds each slot's command once, at first sync,
        // and later only retitles it. A command that changed with state
        // would run the old one under the new title.
        let commands = |app: &Workbench| -> Vec<Option<Command>> {
            app.command_menus()
                .into_iter()
                .flat_map(|menu| menu.items)
                .map(|item| item.map(|item| item.command))
                .collect()
        };
        let mut app = bench();
        let camera = liveness::streaming_camera(0, 0, 0.0);
        app.snapshot.connected = Some(camera.info.clone());
        app.snapshot.active_camera = Some(camera.info.id.clone());
        let mut other = camera.clone();
        other.info.id = "sim:1".into();
        app.snapshot.cameras = vec![camera, other];
        let idle = commands(&app);
        app.snapshot.cameras[1].forwarding = Some("/tmp/sim-1.mp4".into());
        assert_eq!(commands(&app), idle);
    }

    #[test]
    fn the_inspector_check_follows_what_shows() {
        let mut app = bench();
        for (open, peek, image, width) in [
            (true, false, false, 1600.0),
            (false, true, false, 1600.0),
            (true, false, true, 1600.0),
            (false, true, false, 700.0),
            (true, false, false, 700.0),
        ] {
            app.inspector_open = open;
            app.inspector_peek = peek;
            app.image_mode = image;
            app.width = width;
            let shown = item(&app, |c| *c == Command::Action(Action::ToggleInspector));
            assert_eq!(
                shown.checked,
                app.inspector_shown(),
                "{open} {peek} {image} {width}"
            );
        }
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
