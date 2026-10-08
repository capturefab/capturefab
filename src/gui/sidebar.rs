//! The camera list: discovered, connected and recent cameras, and the window footer.
use super::*;
use iced::widget::column;

/// How many failed connect targets the camera list remembers.
const FAILURES_KEPT: usize = 8;

/// The sidebar-welcome package's own state: the camera list and the welcome
/// screen. Add fields here, register their motions below and point them in
/// `sync_side`.
#[derive(Default)]
pub(super) struct SideState {
    /// The last discovery's problems, each in full: its warnings, or the
    /// error it failed with. Replaced whenever a discovery finishes.
    #[allow(dead_code)] // adopted by the sidebar-welcome package
    pub(super) discovery_issues: Vec<String>,
    /// Connect targets (IDs or addresses) whose last attempt failed, with
    /// the whole error and when. Kept by `failed()`; a retry, a success or
    /// editing the address clears one.
    pub(super) connect_failures: HashMap<String, (String, Instant)>,
}

impl SideState {
    super::motion::registry! {
        motions: [],
        flashes: [],
    }

    /// Keep `error` for `target`, forgetting the oldest beyond `FAILURES_KEPT`.
    pub(super) fn connect_failed(&mut self, target: &str, error: String, now: Instant) {
        self.connect_failures
            .insert(target.to_owned(), (error, now));
        while self.connect_failures.len() > FAILURES_KEPT {
            let Some(oldest) = self
                .connect_failures
                .iter()
                .min_by_key(|(_, (_, at))| *at)
                .map(|(target, _)| target.clone())
            else {
                break;
            };
            self.connect_failures.remove(&oldest);
        }
    }
}

/// The sidebar-welcome package's hooks into the shared update cycle; empty
/// until it needs them.
impl Workbench {
    /// Point the sidebar-welcome package's motions at what they show; from
    /// `sync_animations`.
    pub(super) fn sync_side(&mut self) {}

    /// The sidebar-welcome package's bookkeeping on the slow tick, after the
    /// snapshot refresh; from `tick()`.
    pub(super) fn tick_side(&mut self) {}

    /// A command finished, after the shared bookkeeping (`finished`,
    /// `failed`) and before its notice; from `settle()`.
    pub(super) fn result_side(
        &mut self,
        _pending: &Pending,
        _result: &anyhow::Result<serde_json::Value>,
    ) {
    }

    /// Take a screenshot scene word the sidebar-welcome package owns: `late` is
    /// false while the scene is set up and true once its cameras stream.
    /// Returns whether the word was taken; see `apply_scene`.
    pub(super) fn scene_side(&mut self, _word: &str, _late: bool) -> bool {
        false
    }
}

impl Workbench {
    pub(super) fn sidebar(&self, p: &'static Palette) -> Element<'_, Message> {
        let mut devices: Vec<&CameraInfo> = self.snapshot.devices.iter().collect();
        for camera in &self.snapshot.cameras {
            if !devices.iter().any(|device| device.id == camera.info.id) {
                devices.push(&camera.info);
            }
        }
        let discovering = self.pending("Discovering cameras");
        let brand = row![
            icon(Icon::Mark, 18.0, p.accent),
            text("Capturefab").size(style::TITLE).font(style::BOLD),
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        let header = row![
            text("Cameras")
                .size(style::CAPTION)
                .font(style::SEMIBOLD)
                .color(p.secondary),
            text(if discovering {
                "Searching…".to_string()
            } else {
                devices.len().to_string()
            })
            .size(style::CAPTION)
            .color(p.tertiary),
            space::horizontal(),
            tip(
                button(icon(
                    Icon::Refresh,
                    14.0,
                    if discovering {
                        fade(p.accent, self.pulse())
                    } else {
                        p.secondary
                    },
                ))
                .padding(5)
                .style(style::plain)
                .on_press_maybe((!discovering).then_some(Message::Discover)),
                Action::Discover.hint(
                    "Find host, GigE Vision, USB3 Vision and ONVIF cameras",
                    Os::CURRENT
                ),
            ),
        ]
        .spacing(6)
        .align_y(Alignment::Center);
        let mut list = column![].spacing(2);
        if devices.is_empty() {
            list = list.push(
                container(
                    text(if discovering {
                        "Looking for cameras…"
                    } else {
                        "No cameras found"
                    })
                    .size(style::SMALL)
                    .color(p.secondary),
                )
                .padding([8, 10]),
            );
        }
        let recent: Vec<&Recent> = self
            .recent
            .iter()
            .filter(|recent| {
                !devices.iter().any(|device| {
                    device.id == recent.target
                        || device.address.as_deref() == Some(recent.target.as_str())
                })
            })
            .collect();
        for camera in devices {
            let active = self.snapshot.active_camera.as_ref() == Some(&camera.id);
            let state = self
                .snapshot
                .cameras
                .iter()
                .find(|c| c.info.id == camera.id);
            let connecting = self.connecting(&camera.id);
            let color = match state {
                Some(state) if state.streaming => p.live,
                Some(_) => p.accent,
                None if connecting => fade(p.accent, self.pulse()),
                None => p.tertiary,
            };
            let detail = if connecting {
                "Connecting…".to_string()
            } else {
                let place = match &camera.address {
                    Some(address) => {
                        format!("{} · {}", camera.transport, redact_address(address))
                    }
                    None => format!("{} · S/N {}", camera.transport, camera.serial),
                };
                if state.is_some_and(|s| s.streaming) {
                    format!("Streaming · {place}")
                } else {
                    place
                }
            };
            let mut line = row![
                dot(color, 8.0),
                column![
                    clipped(
                        text(camera.model.clone())
                            .size(style::BODY)
                            .font(if active {
                                style::SEMIBOLD
                            } else {
                                style::MEDIUM
                            })
                    ),
                    clipped(
                        text(detail)
                            .size(style::CAPTION)
                            .color(p.secondary)
                            .wrapping(text::Wrapping::None)
                    ),
                ]
                .spacing(1)
                .width(Fill),
            ]
            .spacing(10)
            .align_y(Alignment::Center);
            let hint = if let Some(state) = state {
                format!(
                    "{} {} · {}",
                    camera.vendor,
                    camera.model,
                    if state.streaming {
                        "streaming"
                    } else {
                        "connected"
                    }
                )
            } else {
                format!("Connect to {} {}", camera.vendor, camera.model)
            };
            if state.is_some() {
                line = line.push(tip(
                    button(icon(Icon::Eject, 13.0, p.tertiary))
                        .padding(4)
                        .style(style::plain)
                        .on_press(Message::Disconnect(camera.id.clone())),
                    "Disconnect",
                ));
            } else if !connecting {
                line = line.push(icon(Icon::Plug, 13.0, p.tertiary));
            }
            list = list.push(tip(
                button(line)
                    .width(Fill)
                    .padding([7, 10])
                    .style(style::row(active))
                    .on_press(Message::CameraRow(camera.id.clone())),
                hint,
            ));
        }
        if !recent.is_empty() {
            list = list.push(
                container(
                    text("Recent")
                        .size(style::CAPTION)
                        .font(style::SEMIBOLD)
                        .color(p.secondary),
                )
                .padding(iced::Padding {
                    top: 14.0,
                    right: 4.0,
                    bottom: 4.0,
                    left: 4.0,
                }),
            );
        }
        for entry in recent {
            let connecting = self.connecting(&entry.target);
            let line = row![
                icon(
                    Icon::Recent,
                    13.0,
                    if connecting {
                        fade(p.accent, self.pulse())
                    } else {
                        p.tertiary
                    }
                ),
                column![
                    clipped(text(entry.label.clone()).size(style::BODY)),
                    clipped(
                        text(if connecting {
                            "Connecting…".to_string()
                        } else {
                            entry.detail.clone()
                        })
                        .size(style::CAPTION)
                        .color(p.secondary)
                        .wrapping(text::Wrapping::None)
                    ),
                ]
                .spacing(1)
                .width(Fill),
                tip(
                    button(icon(Icon::Close, 11.0, p.tertiary))
                        .padding(5)
                        .style(style::plain)
                        .on_press(Message::Forget(entry.target.clone())),
                    "Forget",
                ),
            ]
            .spacing(10)
            .align_y(Alignment::Center);
            list = list.push(tip(
                button(line)
                    .width(Fill)
                    .padding([7, 10])
                    .style(style::row(false))
                    .on_press(Message::CameraRow(entry.target.clone())),
                format!("Reconnect {}", entry.label),
            ));
        }
        let appearance = match self.appearance {
            Appearance::System => (Icon::Contrast, "Appearance: match system"),
            Appearance::Light => (Icon::Sun, "Appearance: light"),
            Appearance::Dark => (Icon::Moon, "Appearance: dark"),
        };
        let session_copied = self.copied.is_some() && self.just_copied(&self.session_command());
        let footer = column![
            text_input("Add camera, IP or stream URL", &self.address)
                .id("connect-address")
                .icon(icon::input_icon(Icon::Plus))
                .on_input(Message::Address)
                .on_submit(Message::ConnectAddress)
                .size(style::BODY)
                .padding([7, 10])
                .style(style::input),
            text("GigE · USB3 · RTSP · HTTP · RTMP · native")
                .size(style::CAPTION)
                .color(p.tertiary),
            checkbox(
                "Include simulated cameras",
                self.include_simulator,
                Message::IncludeSimulator,
            ),
            row![
                tip(
                    button(icon(appearance.0, 15.0, p.secondary))
                        .padding(6)
                        .style(style::plain)
                        .on_press(Message::CycleAppearance),
                    appearance.1,
                ),
                tip(
                    button(if session_copied {
                        icon(Icon::Check, 15.0, p.live)
                    } else {
                        icon(Icon::Copy, 15.0, p.secondary)
                    })
                    .padding(6)
                    .style(style::plain)
                    .on_press(Message::CopySessionCommand),
                    if session_copied {
                        "Copied".to_owned()
                    } else {
                        Action::CopySessionCommand.hint(
                            "Copy a command that controls this visible session",
                            Os::CURRENT,
                        )
                    },
                ),
                tip(
                    button(icon(Icon::Help, 15.0, p.secondary))
                        .padding(6)
                        .style(style::plain)
                        .on_press(Message::Help(true)),
                    Action::Help.hint("Keyboard shortcuts and quick guide", Os::CURRENT),
                ),
                space::horizontal(),
                clipped(
                    text(format!("session {}", self.session))
                        .size(style::CAPTION)
                        .font(style::MONO)
                        .color(p.tertiary)
                ),
            ]
            .spacing(2)
            .align_y(Alignment::Center),
        ]
        .spacing(9);
        container(
            column![
                // Beside the macOS traffic lights, which sit at the left.
                self.titlebar(
                    container(brand)
                        .padding(iced::Padding {
                            left: (self.lights() - 12.0).max(6.0),
                            ..iced::Padding::ZERO
                        })
                        .into()
                ),
                space().height(10),
                container(header).padding([0, 4]),
                space().height(4),
                scrollable(list).height(Fill).style(style::scroll),
                footer,
            ]
            .padding(iced::Padding {
                top: 0.0,
                right: 12.0,
                bottom: 12.0,
                left: 12.0,
            }),
        )
        .width(SIDEBAR)
        .height(Fill)
        .style(style::sidebar)
        .into()
    }
}
