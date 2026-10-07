//! The single-camera stage: the title bar, the live image edge to edge with
//! its controls, scopes and status floating over it, and the welcome screen.
use super::*;
use iced::widget::column;

/// A rounded status label: a dot, a word and an optional live figure.
pub(super) fn status_pill<'a>(
    label: &'a str,
    figure: Option<String>,
    chart: Option<Element<'a, Message>>,
    color: Color,
    pulse: f32,
) -> Element<'a, Message> {
    let mut content = row![
        dot(fade(color, pulse), 6.0),
        text(label).size(style::SMALL).font(style::MEDIUM),
    ]
    .spacing(6)
    .align_y(Alignment::Center);
    if let Some(chart) = chart {
        content = content.push(chart);
    }
    if let Some(figure) = figure {
        // A fixed slot, so a changing figure never shifts what follows.
        content = content.push(text(figure).size(style::SMALL).width(58));
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
        let sidebar = self.sidebar_slide.interpolate(0.0f32, SIDEBAR, self.now);
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
            .height(self.chrome.interpolate(0.0f32, BAR, self.now))
            .clip(true)
            .into()
    }

    pub(super) fn single_view(&self, p: &'static Palette) -> Element<'_, Message> {
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
                Some(fps_spark(history, p.live, p.warn, (64.0, 14.0))),
                p.live,
                self.pulse(),
            ))
        } else if connected {
            Some(status_pill("Ready", None, None, p.accent_text, 1.0))
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
                        p,
                    ),
                    Action::ToggleStream.hint("Start or stop acquisition", os),
                ));
        }
        let title = snapshot
            .connected
            .as_ref()
            .map_or(String::new(), |camera| camera.model.clone());
        let header = self.title_bar(title, status, actions.into(), p);
        let body = match &self.shown {
            Some(shown) => self.stage(shown),
            None if !connected => self.welcome(p),
            None => self.ready(p),
        };
        column![header, body].into()
    }

    /// The live image filling the stage, with what floats over it.
    fn stage<'a>(&'a self, shown: &'a Shown) -> Element<'a, Message> {
        let snapshot = &self.snapshot;
        let image = responsive(move |size| {
            self.stage.set(size);
            shown.view(&self.gpu, (!self.fit).then_some(self.zoom))
        });
        let mut layers = stack![
            container(image)
                .width(Fill)
                .height(Fill)
                .style(style::stage)
        ];
        if let Some(region) = self.focus_overlay(shown) {
            layers = layers.push(region);
        }
        let mut badges = row![].spacing(6).align_y(Alignment::Center);
        if !snapshot.streaming {
            badges = badges.push(last_frame());
        }
        if let Some(error) = self.display_error.as_ref().or(snapshot.last_error.as_ref()) {
            badges = badges.push(tip(
                container(
                    row![
                        icon(Icon::Warning, 12.0, style::DARK.danger),
                        text(error.clone()).size(style::SMALL)
                    ]
                    .spacing(6)
                    .align_y(Alignment::Center),
                )
                .max_width(520)
                .clip(true)
                .padding([3, 8])
                .style(style::badge),
                error.clone(),
            ));
        }
        layers = layers.push(container(badges).padding(12));
        if let Some(scopes) = self.scopes() {
            layers = layers.push(scopes);
        }
        let controls = self.controls.interpolate(0.0f32, 1.0, self.now);
        if controls > 0.01 {
            layers = layers.push(
                container(
                    mouse_area(self.stage_controls(controls))
                        .on_enter(Message::OverControls(true))
                        .on_exit(Message::OverControls(false)),
                )
                .center_x(Fill)
                .align_bottom(Fill)
                .padding(iced::Padding {
                    bottom: 16.0 + motion::rise(6.0, controls),
                    ..iced::Padding::new(16.0)
                }),
            );
        }
        let flash = self.shutter.interpolate(0.0f32, 0.25, self.now);
        if flash > 0.0 {
            layers = layers.push(container(space().width(Fill).height(Fill)).style(move |_| {
                container::Style::default().background(Color {
                    a: flash,
                    ..Color::WHITE
                })
            }));
        }
        mouse_area(layers)
            .on_move(|_| Message::Pointer)
            .on_double_click(Message::ToggleImage)
            .into()
    }

    /// Zoom, scope and image mode controls with the frame's details, on
    /// dark glass at the bottom of the stage; `shown` fades them.
    fn stage_controls(&self, shown: f32) -> Element<'_, Message> {
        let os = Os::CURRENT;
        let snapshot = &self.snapshot;
        let zoom_actual = !self.fit && (self.zoom - 1.0).abs() < 0.01;
        let ink = |active: bool| {
            fade(
                if active {
                    Palette::of(self.dark()).accent
                } else {
                    style::ON_STAGE_SECONDARY
                },
                shown,
            )
        };
        let segment = |label: &'static str, selected: bool, on: Message| {
            button(text(label).size(style::SMALL).font(if selected {
                style::SEMIBOLD
            } else {
                style::SANS
            }))
            .padding([3, 10])
            .style(style::glass_segment(selected, shown))
            .on_press(on)
        };
        let glass = |kind: Icon, active: bool, on: Message| {
            button(icon(kind, 14.0, ink(active)))
                .padding(5)
                .style(style::on_glass(active, shown))
                .on_press(on)
        };
        let (label, lost) = snapshot
            .transport
            .as_ref()
            .map_or(("dropped", snapshot.dropped), |stats| {
                ("lost", transport_loss(stats))
            });
        let mut details = vec![];
        if let Some((_, width, height, format, _)) = self.frame_meta {
            details.push(format!("{width} × {height}"));
            details.push(pixel_name(format));
        }
        details.push(format!("{} frames", grouped(snapshot.frames)));
        details.push(format!("{lost} {label}"));
        let bar = row![
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
            .spacing(2),
            tip(
                glass(Icon::Minus, false, Message::Zoom(1.0 / 1.25)),
                Action::ZoomOut.hint("Zoom out", os)
            ),
            tip(
                glass(Icon::Plus, false, Message::Zoom(1.25)),
                Action::ZoomIn.hint("Zoom in", os)
            ),
            tip(
                glass(Icon::Chart, self.exposure_open, Message::ToggleExposure),
                Action::ToggleExposure.hint("Exposure histogram", os)
            ),
            tip(
                glass(Icon::Scan, self.focus.open, Message::ToggleFocusRegion),
                Action::ToggleFocusRegion.hint("Focus region", os)
            ),
            space::horizontal(),
            clipped(
                text(details.join("  ·  "))
                    .size(style::SMALL)
                    .color(fade(
                        if lost > 0 {
                            Palette::of(self.dark()).warn
                        } else {
                            style::ON_STAGE_SECONDARY
                        },
                        shown,
                    ))
                    .wrapping(text::Wrapping::None)
            ),
            tip(
                glass(
                    if self.image_mode {
                        Icon::CornersIn
                    } else {
                        Icon::CornersOut
                    },
                    false,
                    Message::ToggleImage
                ),
                Action::ImageMode.hint(
                    if self.image_mode {
                        "Leave image only"
                    } else {
                        "Image only"
                    },
                    os
                )
            ),
        ]
        .spacing(6)
        .align_y(Alignment::Center);
        container(bar)
            .max_width(640)
            .width(Fill)
            .padding([6, 8])
            .style(style::overlay(shown))
            .into()
    }

    /// A connected camera before its first frame.
    fn ready(&self, p: &'static Palette) -> Element<'_, Message> {
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
