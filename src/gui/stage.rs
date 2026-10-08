//! The single-camera stage: the live image edge to edge with its controls,
//! scopes and status floating over it.
use super::titlebar::{fps_spark, status_pill};
use super::*;
use iced::widget::column;

impl Workbench {
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
                1.0,
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
                        icon(Icon::Warning, 12.0, style::STAGE.danger),
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
        let controls = self.controls.get(self.now);
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
        let flash = self.shutter.lerp(0.0, 0.25, self.now);
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
}
