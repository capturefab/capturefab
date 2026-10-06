//! The camera list: discovered, connected and recent cameras, and the window footer.
use super::*;
use iced::widget::column;

impl Workbench {
    pub(super) fn sidebar(&self, p: &'static Palette) -> Element<'_, Message> {
        let mut devices = self.snapshot.devices.clone();
        for camera in &self.snapshot.cameras {
            if !devices.iter().any(|device| device.id == camera.info.id) {
                devices.push(camera.info.clone());
            }
        }
        let discovering = self.pending("Discovering cameras");
        let brand = row![
            icon(Icon::Mark, 20.0, p.accent),
            text("Capturefab").size(15).font(style::BOLD),
        ]
        .spacing(9)
        .align_y(Alignment::Center);
        let header = row![
            text("Cameras")
                .size(style::SMALL)
                .font(style::SEMIBOLD)
                .color(p.secondary),
            text(if discovering {
                "Searching…".to_string()
            } else {
                devices.len().to_string()
            })
            .size(style::SMALL)
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
                Action::Discover.hint("Find GigE Vision and USB3 Vision devices", Os::CURRENT),
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
                Some(state) if state.streaming => fade(p.live, self.pulse()),
                Some(_) => p.accent,
                None if connecting => fade(p.accent, self.pulse()),
                None => p.tertiary,
            };
            let detail = if connecting {
                "Connecting…".to_string()
            } else {
                match &camera.address {
                    Some(address) => {
                        format!("{} · {}", camera.transport, redact_address(address))
                    }
                    None => format!("{} · S/N {}", camera.transport, camera.serial),
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
            let hint = if state.is_some() {
                format!("{} {} · {}", camera.vendor, camera.model, "connected")
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
                        .size(style::SMALL)
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
                    button(icon(Icon::Copy, 15.0, p.secondary))
                        .padding(6)
                        .style(style::plain)
                        .on_press(Message::CopySessionCommand),
                    Action::CopySessionCommand.hint(
                        "Copy a command that controls this visible session",
                        Os::CURRENT,
                    ),
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
                self.titlebar(container(brand).padding([0, 6]).into()),
                space().height(22),
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
