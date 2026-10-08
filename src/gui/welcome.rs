//! What the stage shows before pictures: the welcome screen with no camera
//! connected, and a connected camera before its first frame.
use super::*;
use iced::widget::column;

impl Workbench {
    /// A connected camera before its first frame.
    pub(super) fn ready(&self, p: &'static Palette) -> Element<'_, Message> {
        let streaming = self.snapshot.streaming;
        let mut ready = column![
            icon(Icon::Camera, 40.0, style::ON_STAGE_SECONDARY),
            space().height(6),
            text(if streaming {
                "Waiting for the first frame…"
            } else {
                "Ready for your first frame"
            })
            .size(style::TITLE)
            .font(style::SEMIBOLD)
            .color(style::ON_STAGE),
        ]
        .spacing(6)
        .align_x(Alignment::Center);
        if !streaming {
            ready = ready.push(space().height(8)).push(
                row![
                    stream_button(false, "Start stream", Some(Message::ToggleStream), p)
                        .padding([7, 16]),
                    button(text("Capture one frame").size(style::BODY))
                        .padding([7, 12])
                        .style(style::on_glass(false, 1.0))
                        .on_press_maybe(
                            (!self.pending("Saving capture")).then_some(Message::Capture)
                        ),
                ]
                .spacing(10)
                .align_y(Alignment::Center),
            );
        }
        container(center(ready))
            .width(Fill)
            .height(Fill)
            .style(style::stage)
            .into()
    }

    pub(super) fn welcome(&self, p: &'static Palette) -> Element<'_, Message> {
        let t = self.welcome.interpolate(0.0f32, 1.0, self.now);
        let discovering = self.pending("Discovering cameras");
        let found: Vec<_> = self
            .snapshot
            .devices
            .iter()
            .filter(|device| {
                !self
                    .snapshot
                    .cameras
                    .iter()
                    .any(|camera| camera.info.id == device.id)
            })
            .take(4)
            .collect();
        let recent: Vec<_> = self
            .recent
            .iter()
            .filter(|recent| {
                !self.snapshot.devices.iter().any(|device| {
                    device.id == recent.target
                        || device.address.as_deref() == Some(recent.target.as_str())
                })
            })
            .take(4 - found.len())
            .collect();
        let subtitle = if discovering {
            "Looking for GigE Vision and USB3 Vision cameras…".to_string()
        } else if found.is_empty() {
            "No cameras found yet. Try the simulator, or enter an address or stream URL.".into()
        } else if found.len() == 1 {
            "Found 1 camera. Click it to connect.".into()
        } else {
            format!("Found {} cameras. Click one to connect.", found.len())
        };
        let card = |kind: Icon, title: String, detail: String, target: String| {
            let connecting = self.connecting(&target);
            button(
                row![
                    icon(
                        kind,
                        20.0,
                        fade(p.accent, t * if connecting { self.pulse() } else { 1.0 })
                    ),
                    column![
                        clipped(
                            text(title)
                                .size(style::BODY)
                                .font(style::MEDIUM)
                                .color(fade(p.text, t))
                        ),
                        clipped(
                            text(if connecting {
                                "Connecting…".to_string()
                            } else {
                                detail
                            })
                            .size(style::CAPTION)
                            .color(fade(p.secondary, t))
                        ),
                    ]
                    .spacing(2)
                    .width(Fill),
                    icon(Icon::ChevronRight, 13.0, fade(p.tertiary, t)),
                ]
                .spacing(12)
                .align_y(Alignment::Center),
            )
            .width(Fill)
            .padding([10, 14])
            .style(style::card)
            .on_press(Message::CameraRow(target))
        };
        let mut cards = column![].spacing(8).width(Fill);
        for device in &found {
            let detail = match &device.address {
                Some(address) => format!("{} · {}", device.transport, redact_address(address)),
                None => format!("{} · S/N {}", device.transport, device.serial),
            };
            cards = cards.push(card(
                Icon::transport(device.transport),
                device.model.clone(),
                detail,
                device.id.clone(),
            ));
        }
        if !recent.is_empty() {
            cards = cards.push(
                container(
                    text("Recent")
                        .size(style::SMALL)
                        .font(style::SEMIBOLD)
                        .color(fade(p.secondary, t)),
                )
                .padding(iced::Padding {
                    top: if found.is_empty() { 0.0 } else { 8.0 },
                    left: 2.0,
                    ..iced::Padding::ZERO
                }),
            );
        }
        for entry in &recent {
            cards = cards.push(card(
                Icon::transport(entry.transport),
                entry.label.clone(),
                entry.detail.clone(),
                entry.target.clone(),
            ));
        }
        let simulator_first = found.is_empty() && recent.is_empty() && !discovering;
        let action = |kind: Icon, label: &'static str, on: Option<Message>, primary: bool| {
            button(
                row![
                    icon(kind, 15.0, if primary { Color::WHITE } else { p.secondary }),
                    text(label).size(style::BODY).font(style::MEDIUM),
                ]
                .spacing(7)
                .align_y(Alignment::Center),
            )
            .padding([7, 12])
            .style(if primary {
                style::primary
            } else {
                style::secondary
            })
            .on_press_maybe(on)
        };
        let os = Os::CURRENT;
        let actions = row![
            tip(
                action(
                    Icon::Refresh,
                    "Search again",
                    (!discovering).then_some(Message::Discover),
                    false
                ),
                Action::Discover.hint("Find GigE Vision and USB3 Vision devices", os),
            ),
            action(
                Icon::Cube,
                "Try a simulated camera",
                (!self.connecting("sim:0")).then_some(Message::TrySimulator),
                simulator_first,
            ),
            tip(
                action(
                    Icon::Plus,
                    "Enter an address",
                    Some(Message::EnterAddress),
                    false
                ),
                Action::ConnectAddress.hint("IP address, RTSP, SRT or HTTP stream URL", os),
            ),
        ]
        .spacing(8)
        .wrap()
        .vertical_spacing(8);
        let mut content = column![
            icon(Icon::Mark, 44.0, fade(p.accent, t)),
            space().height(10),
            text("Connect a camera")
                .size(style::DISPLAY)
                .font(style::BOLD)
                .color(fade(p.text, t)),
            text(subtitle)
                .size(style::BODY)
                .color(fade(p.secondary, t))
                .align_x(Alignment::Center),
            space().height(14),
        ]
        .spacing(4)
        .align_x(Alignment::Center)
        .max_width(460);
        if !found.is_empty() || !recent.is_empty() {
            content = content.push(cards).push(space().height(14));
        }
        content = content.push(actions).push(space().height(18)).push(
            text(format!(
                "GigE cameras need an address on this computer's subnet. USB3 cameras need operating system access. {} opens the quick guide.",
                Action::Help.shortcut(os)
            ))
            .size(style::CAPTION)
            .color(fade(p.tertiary, t))
            .align_x(Alignment::Center),
        );
        center(container(content).padding(iced::Padding {
            top: motion::rise(24.0, t),
            right: 24.0,
            bottom: 0.0,
            left: 24.0,
        }))
        .clip(true)
        .into()
    }
}
