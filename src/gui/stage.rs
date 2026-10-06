//! The single-camera stage: header, live image, zoom controls, histogram and the welcome screen.
use super::*;
use iced::widget::column;

impl Workbench {
    pub(super) fn header<'a>(
        &self,
        marker: Element<'a, Message>,
        title: String,
        subtitle: Element<'a, Message>,
        actions: Element<'a, Message>,
    ) -> Element<'a, Message> {
        self.titlebar(
            container(
                row![
                    column![
                        row![
                            marker,
                            clipped(text(title).size(style::TITLE).font(style::BOLD))
                        ]
                        .spacing(12)
                        .align_y(Alignment::Center),
                        subtitle,
                    ]
                    .spacing(4)
                    .width(Fill),
                    actions,
                ]
                .spacing(16)
                .align_y(Alignment::Center),
            )
            .padding([0.0, GUTTER])
            .into(),
        )
    }

    pub(super) fn single_view(&self, p: &'static Palette) -> Element<'_, Message> {
        let snapshot = &self.snapshot;
        let connected = snapshot.connected.is_some();
        let (status, color) = if snapshot.streaming {
            ("Streaming", p.live)
        } else if connected {
            ("Ready", p.accent_text)
        } else {
            ("No camera connected", p.secondary)
        };
        let mut subtitle = row![
            text(status)
                .size(style::BODY)
                .font(style::MEDIUM)
                .color(color)
        ]
        .spacing(6)
        .align_y(Alignment::Center);
        if connected {
            let (label, lost) = snapshot
                .transport
                .as_ref()
                .map_or(("dropped", snapshot.dropped), |stats| {
                    ("lost", transport_loss(stats))
                });
            let mut parts = vec![
                format!("{:.1} fps", snapshot.fps),
                format!("{} frames", grouped(snapshot.frames)),
            ];
            parts.push(format!("{lost} {label}"));
            for part in parts {
                subtitle = subtitle
                    .push(text("·").size(style::BODY).color(p.tertiary))
                    .push(text(part).size(style::BODY).color(p.secondary));
            }
            if lost > 0 {
                subtitle = subtitle.push(dot(p.warn, 6.0));
            }
            if snapshot.auto.is_some() {
                subtitle = subtitle
                    .push(text("·").size(style::BODY).color(p.tertiary))
                    .push(text("Auto").size(style::BODY).color(p.accent_text));
            }
        }
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
                .padding([7, 12])
                .style(style::secondary)
                .on_press(Message::Focus(false)),
                Action::Overview.hint("Back to all cameras", Os::CURRENT),
            ));
        }
        if connected {
            actions = actions.push(tip(
                stream_button(
                    snapshot.streaming,
                    if snapshot.streaming { "Stop" } else { "Start" },
                    Some(Message::ToggleStream),
                    p,
                ),
                Action::ToggleStream.hint("Start or stop acquisition", Os::CURRENT),
            ));
        }
        let title = snapshot
            .connected
            .as_ref()
            .map_or("Live Preview".to_string(), |camera| camera.model.clone());
        let marker = icon(
            Icon::Camera,
            24.0,
            if connected { p.accent } else { p.tertiary },
        );
        let header = self.header(marker, title, subtitle.into(), actions.into());
        let stage: Element<'_, Message> = match &self.shown {
            Some(shown) => {
                let mut layers = stack![responsive(move |size| {
                    self.stage.set(size);
                    shown.view(&self.gpu, (!self.fit).then_some(self.zoom))
                })];
                if !snapshot.streaming {
                    layers = layers.push(container(last_frame()).padding(12));
                }
                layers.into()
            }
            None if !connected => self.welcome(p),
            None => {
                let mut ready = column![
                    icon(Icon::Camera, 46.0, p.tertiary),
                    space().height(6),
                    text(if snapshot.streaming {
                        "Waiting for the first frame…"
                    } else {
                        "Ready for your first frame"
                    })
                    .size(18)
                    .font(style::SEMIBOLD),
                ]
                .spacing(6)
                .align_x(Alignment::Center);
                if !snapshot.streaming {
                    ready = ready.push(space().height(8)).push(
                        row![
                            stream_button(false, "Start stream", Some(Message::ToggleStream), p),
                            button(text("Capture one frame").size(style::BODY))
                                .padding([7, 12])
                                .style(style::link)
                                .on_press_maybe(
                                    (!self.pending("Saving capture")).then_some(Message::Capture)
                                ),
                        ]
                        .spacing(10)
                        .align_y(Alignment::Center),
                    );
                }
                center(ready).into()
            }
        };
        let flash = self.shutter.interpolate(0.0f32, 0.55, self.now);
        let stage: Element<'_, Message> = if flash > 0.0 {
            stack![
                stage,
                container(space().width(Fill).height(Fill)).style(move |_| {
                    container::Style::default().background(Color {
                        a: flash,
                        ..Color::WHITE
                    })
                }),
            ]
            .into()
        } else {
            stage
        };
        let os = Os::CURRENT;
        let zoom_actual = !self.fit && (self.zoom - 1.0).abs() < 0.01;
        let mut controls = row![
            container(
                row![
                    tip(
                        segment("Fit", self.fit, Message::Fit),
                        Action::ZoomFit.hint("Zoom to fit", os)
                    ),
                    tip(
                        segment("1:1", zoom_actual, Message::Actual),
                        Action::ZoomActual.hint("Actual pixels", os)
                    ),
                ]
                .spacing(2)
            )
            .padding(2)
            .style(style::segment_track),
            tip(
                icon_button(Icon::Minus, 13.0, p.secondary, Message::Zoom(1.0 / 1.25)),
                Action::ZoomOut.hint("Zoom out", os)
            ),
            tip(
                icon_button(Icon::Plus, 13.0, p.secondary, Message::Zoom(1.25)),
                Action::ZoomIn.hint("Zoom in", os)
            ),
            tip(
                icon_button(
                    Icon::Chart,
                    14.0,
                    if self.histogram_open {
                        p.accent
                    } else {
                        p.secondary
                    },
                    Message::ToggleHistogram,
                ),
                "Luminance histogram"
            ),
            space::horizontal(),
        ]
        .spacing(6)
        .align_y(Alignment::Center);
        if let Some((id, width, height, format, _)) = self.frame_meta {
            controls = controls.push(
                text(format!(
                    "{width} × {height}  ·  {}  ·  #{id}",
                    pixel_name(format)
                ))
                .size(style::CAPTION)
                .font(style::MONO)
                .color(p.secondary),
            );
        }
        let mut content = column![
            header,
            space().height(18),
            container(stage)
                .width(Fill)
                .height(Fill)
                .clip(true)
                .style(style::stage),
        ];
        if self.shown.is_some() {
            content = content.push(space().height(10)).push(controls);
        }
        let histogram = self.histogram_slide.interpolate(0.0f32, 52.0, self.now);
        if histogram > 0.5 && self.shown.is_some() {
            content = content.push(
                container(
                    column![
                        space().height(8),
                        iced::widget::canvas(preview::Histogram {
                            bins: self.histogram,
                            color: Color {
                                a: 0.55,
                                ..p.accent
                            },
                        })
                        .width(Fill)
                        .height(44),
                    ]
                    .height(52),
                )
                .height(histogram)
                .clip(true),
            );
        }
        if let Some(error) = self.display_error.as_ref().or(snapshot.last_error.as_ref()) {
            content = content.push(space().height(6)).push(tip(
                clipped(text(error.clone()).size(style::SMALL).color(p.danger)),
                error.clone(),
            ));
        }
        content
            .push(space().height(12))
            .padding(iced::Padding {
                top: 0.0,
                right: GUTTER,
                bottom: 0.0,
                left: GUTTER,
            })
            .height(Fill)
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
                .size(22)
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
            top: 24.0 * (1.0 - t),
            right: 24.0,
            bottom: 0.0,
            left: 24.0,
        }))
        .clip(true)
        .into()
    }
}
