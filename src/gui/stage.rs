//! The single-camera stage: the live image edge to edge with its controls,
//! scopes and status floating over it.
use super::*;
use iced::widget::column;

/// The stage package's own state: the single-camera stage, the overview grid
/// and the scopes. Add fields here, register their motions below and point them
/// in `sync_stage`. It derives `Default` while empty; write `Default` by hand
/// once it holds a `Motion`.
#[derive(Default)]
pub(super) struct StageState {}

impl StageState {
    super::motion::registry! {
        motions: [],
        flashes: [],
    }
}

/// The stage package's hooks into the shared update cycle; empty until it needs
/// them.
impl Workbench {
    /// Point the stage package's motions at what they show; from
    /// `sync_animations`.
    pub(super) fn sync_stage(&mut self) {}

    /// The stage package's bookkeeping on the slow tick, after the snapshot
    /// refresh; from `tick()`.
    pub(super) fn tick_stage(&mut self) {}

    /// A command finished, after the shared bookkeeping (`finished`,
    /// `failed`) and before its notice; from `settle()`.
    pub(super) fn result_stage(
        &mut self,
        _pending: &Pending,
        _result: &anyhow::Result<serde_json::Value>,
    ) {
    }

    /// Take a screenshot scene word the stage package owns: `late` is false
    /// while the scene is set up and true once its cameras stream. Returns
    /// whether the word was taken; see `apply_scene`.
    pub(super) fn scene_stage(&mut self, _word: &str, _late: bool) -> bool {
        false
    }
}

impl Workbench {
    pub(super) fn single_view(&self, p: &'static Palette) -> Element<'_, Message> {
        let body = match &self.shown {
            Some(shown) => self.stage(shown),
            None if self.snapshot.connected.is_none() => self.welcome(p),
            None => self.ready(p),
        };
        column![self.single_bar(p), body].into()
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
