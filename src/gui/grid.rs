//! The multi-camera overview: every camera's image as large as the window
//! allows, captioned over the picture, with actions on hover.
use super::stage::stall_badge;
use super::titlebar::fps_spark;
use super::*;
use crate::session::CameraSnapshot;
use iced::widget::canvas::{self, Path};
use iced::widget::column;
use iced::{Point, Rectangle, Renderer, mouse};

const GAP: f32 = 8.0;
const MIN_TILE_HEIGHT: f32 = 140.0;
/// Tiles narrower than this leave out the frame rate's sparkline.
const SPARK_WIDTH: f32 = 320.0;

/// Columns and tile size that show `count` images of `aspect` (width over
/// height) as large as possible in `area`.
pub(super) fn layout(count: usize, aspect: f32, area: Size) -> (usize, Size) {
    let count = count.max(1);
    (1..=count)
        .map(|columns| {
            let rows = count.div_ceil(columns);
            let width = (area.width - GAP * (columns - 1) as f32) / columns as f32;
            let height = (area.height - GAP * (rows - 1) as f32) / rows as f32;
            let width = width.min(height * aspect).max(1.0);
            (columns, Size::new(width, width / aspect))
        })
        .max_by(|a, b| a.1.width.total_cmp(&b.1.width))
        .unwrap_or((1, area))
}

/// Whether each camera shares its model with another, so its caption needs
/// the serial number to tell them apart.
fn twins(cameras: &[CameraSnapshot]) -> Vec<bool> {
    let mut models: HashMap<&str, usize> = HashMap::new();
    for camera in cameras {
        *models.entry(camera.info.model.as_str()).or_default() += 1;
    }
    cameras
        .iter()
        .map(|camera| models[camera.info.model.as_str()] > 1)
        .collect()
}

impl Workbench {
    pub(super) fn overview_view(&self, p: &'static Palette) -> Element<'_, Message> {
        let grid = responsive(move |size| self.grid(size));
        column![
            self.overview_bar(p),
            container(grid)
                .padding(GAP)
                .width(Fill)
                .height(Fill)
                .style(style::stage),
        ]
        .into()
    }

    /// Everything drawn here is on the stage, so it takes its colors from
    /// `style::STAGE` whatever the appearance.
    fn grid(&self, size: Size) -> Element<'_, Message> {
        let cameras = &self.snapshot.cameras;
        let aspect = cameras
            .iter()
            .find_map(|camera| self.previews.get(&camera.info.id)?.meta)
            .map_or(4.0 / 3.0, |(_, width, height, _)| {
                width as f32 / height.max(1) as f32
            });
        let (columns, tile) = layout(cameras.len(), aspect, size);
        let tile = if tile.height < MIN_TILE_HEIGHT {
            Size::new(MIN_TILE_HEIGHT * aspect, MIN_TILE_HEIGHT)
        } else {
            tile
        };
        let twins = twins(cameras);
        let mut grid = column![].spacing(GAP).align_x(Alignment::Center);
        for (chunk, twins) in cameras.chunks(columns).zip(twins.chunks(columns)) {
            let mut line = row![].spacing(GAP);
            for (camera, &twin) in chunk.iter().zip(twins) {
                line = line.push(self.tile(camera, tile, twin));
            }
            grid = grid.push(line);
        }
        scrollable(container(grid).center_x(size.width).center_y(size.height))
            .style(style::scroll)
            .into()
    }

    /// A camera's picture with its caption, badges and, under the pointer,
    /// its actions. `twin` adds the serial number to the caption.
    fn tile<'a>(
        &'a self,
        camera: &'a CameraSnapshot,
        size: Size,
        twin: bool,
    ) -> Element<'a, Message> {
        let id = &camera.info.id;
        let surface = Palette::of(self.dark()).stage;
        let preview = self.previews.get(id);
        let picture: Element<'_, Message> = match preview.and_then(|preview| preview.shown.as_ref())
        {
            Some(shown) => shown.view(&self.gpu, None),
            None => center(
                text(if camera.streaming {
                    "Waiting for a frame…"
                } else {
                    "Ready to stream"
                })
                .size(style::BODY)
                .color(style::STAGE.secondary),
            )
            .into(),
        };
        let picture = button(veil(
            container(picture)
                .width(Fill)
                .height(Fill)
                .clip(true)
                .style(style::tile),
            surface,
            style::RADIUS_MEDIUM,
            self.revealed(id),
        ))
        .padding(0)
        .width(Fill)
        .height(Fill)
        .style(style::bare)
        .on_press(Message::Select(id.clone()));
        let stalled = self.stalled(id);
        let mut layers = stack![
            picture,
            iced::widget::canvas(Corners {
                radius: style::RADIUS_MEDIUM
            })
            .width(Fill)
            .height(Fill),
            container(
                container(self.caption(camera, twin, stalled.is_some(), size.width))
                    .padding([10, 12])
                    .style(style::caption)
            )
            .align_bottom(Fill)
            .width(Fill),
        ];
        let mut badges = row![].spacing(6).align_y(Alignment::Center);
        // What an error's text may take beside the other badges and the
        // tile's actions.
        let mut room = size.width - 150.0;
        if !camera.streaming && preview.is_some_and(|preview| preview.shown.is_some()) {
            badges = badges.push(last_frame());
            room -= 86.0;
        }
        if let Some(silent) = stalled {
            badges = badges.push(stall_badge(silent));
            room -= 170.0;
        }
        if let Some(error) = self.camera_error(id) {
            badges = badges.push(self.error_badge(id, error, room));
        }
        layers = layers.push(container(badges).padding(10));
        let shown = self.tile_controls(id);
        if shown > 0.01 {
            let glass = |kind: Icon, hint: String, on: Message| {
                tip(
                    button(icon(kind, 14.0, fade(style::STAGE.text, shown)))
                        .padding(5)
                        .style(style::on_glass(false, shown))
                        .on_press(on),
                    hint,
                )
            };
            let (play, hint) = if camera.streaming {
                (Icon::Stop, "Stop stream")
            } else {
                (Icon::Play, "Start stream")
            };
            layers = layers.push(
                container(
                    container(
                        row![
                            glass(
                                play,
                                hint.into(),
                                Message::StreamCamera(id.clone(), !camera.streaming),
                            ),
                            glass(
                                Icon::Camera,
                                "Capture with the settings in Capture".into(),
                                Message::CaptureCamera(id.clone()),
                            ),
                            glass(
                                Icon::CornersOut,
                                format!(
                                    "Open · S/N {} · worker PID {}",
                                    camera.info.serial, camera.worker_pid
                                ),
                                Message::FocusTile(id.clone()),
                            ),
                        ]
                        .spacing(2),
                    )
                    .padding(3)
                    .style(style::overlay(shown)),
                )
                .align_right(Fill)
                .padding(8),
            );
        }
        let flash = self.flash(id, false);
        if flash > 0.0 {
            let wash = !motion::reduce_motion();
            layers = layers.push(container(space().width(Fill).height(Fill)).style(move |_| {
                container::Style {
                    background: wash.then(|| fade(Color::WHITE, 0.22 * flash).into()),
                    border: iced::Border {
                        color: fade(Color::WHITE, 0.9 * flash),
                        width: 2.0,
                        radius: style::RADIUS_MEDIUM.into(),
                    },
                    ..container::Style::default()
                }
            }));
        }
        let ring = self.selection_level(id);
        if ring > 0.0 {
            layers =
                layers.push(container(space().width(Fill).height(Fill)).style(style::ring(ring)));
        }
        container(
            mouse_area(layers)
                .on_enter(Message::HoverTile(Some(id.clone())))
                .on_exit(Message::HoverTile(None))
                .on_double_click(Message::FocusTile(id.clone())),
        )
        .width(size.width)
        .height(size.height)
        .into()
    }

    /// The line over the bottom of a tile's picture: whether frames come,
    /// which camera it is, what else it does, and its frame rate.
    fn caption<'a>(
        &'a self,
        camera: &'a CameraSnapshot,
        twin: bool,
        stalled: bool,
        width: f32,
    ) -> Element<'a, Message> {
        let stage = style::STAGE;
        let mut name = row![one_line(
            camera.info.model.as_str(),
            style::BODY,
            style::SEMIBOLD,
            stage.text
        )]
        .spacing(7)
        .align_y(Alignment::Center);
        // The model keeps its room first, so the serial is what gets cut.
        if twin {
            name = name.push(one_line(
                camera.info.serial.as_str(),
                style::SMALL,
                style::SANS,
                stage.secondary,
            ));
        }
        let mut caption = row![
            dot(
                if !camera.streaming {
                    stage.secondary
                } else if stalled {
                    stage.warn
                } else {
                    stage.live
                },
                7.0
            ),
            // Fluid, so the marks and figures after it are laid out first.
            container(name).width(Fill),
        ]
        .spacing(7)
        .align_y(Alignment::Center);
        if let Some(output) = &camera.forwarding {
            caption = caption.push(if output.contains("://") {
                chip(
                    icon(Icon::Broadcast, 12.0, stage.accent_text),
                    format!("Forwarding to {}", redact_address(output)),
                )
            } else {
                chip(dot(stage.danger, 7.0), format!("Recording to {output}"))
            });
        }
        if let Some(auto) = &camera.auto {
            caption = caption.push(chip(
                text("Auto")
                    .size(style::CAPTION)
                    .font(style::SEMIBOLD)
                    .color(stage.accent_text),
                format!("Auto mode · {}", auto_summary(auto)),
            ));
        }
        let lost = liveness::camera_loss(camera);
        if lost > 0 {
            caption = caption.push(self.loss_label(camera, lost, 1.0));
        }
        if camera.streaming && width >= SPARK_WIDTH {
            caption = caption.push(fps_spark(
                self.throughput.get(&camera.info.id),
                stage.live,
                stage.warn,
                (52.0, 14.0),
            ));
        }
        caption
            .push(
                text(format!("{:.1} fps", camera.fps))
                    .size(style::SMALL)
                    .color(stage.secondary)
                    .width(58)
                    .align_x(Alignment::End)
                    .wrapping(text::Wrapping::None),
            )
            .into()
    }
}

/// A small mark in a tile's caption, such as forwarding or auto mode,
/// explained on hover.
fn chip<'a>(content: impl Into<Element<'a, Message>>, about: String) -> Element<'a, Message> {
    tip_above(
        container(content)
            .height(18)
            .align_y(Alignment::Center)
            .padding([0, 6])
            .style(style::badge),
        about,
    )
}

/// Paints the stage outside a rounded rectangle, so the picture under it
/// shows rounded corners on every renderer: a container clips to a
/// rectangle, and the GPU picture has no rounding of its own. Drawn again
/// only when the tile's size or the appearance changes.
struct Corners {
    radius: f32,
}

impl canvas::Program<Message> for Corners {
    type State = (canvas::Cache, Cell<Option<Color>>);

    fn draw(
        &self,
        (cache, painted): &Self::State,
        renderer: &Renderer,
        theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<canvas::Geometry> {
        let color = Palette::of(theme.extended_palette().is_dark).stage;
        if painted.get() != Some(color) {
            cache.clear();
            painted.set(Some(color));
        }
        vec![cache.draw(renderer, bounds.size(), |frame| {
            let (width, height) = (bounds.width, bounds.height);
            let radius = self.radius.min(width / 2.0).min(height / 2.0);
            for (x, y, sx, sy) in [
                (0.0, 0.0, 1.0, 1.0),
                (width, 0.0, -1.0, 1.0),
                (0.0, height, 1.0, -1.0),
                (width, height, -1.0, -1.0),
            ] {
                let corner = Point::new(x, y);
                frame.fill(
                    &Path::new(|path| {
                        path.move_to(corner);
                        path.line_to(Point::new(x + sx * radius, y));
                        path.arc_to(corner, Point::new(x, y + sy * radius), radius);
                        path.close();
                    }),
                    color,
                );
            }
        })]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use liveness::streaming_camera;

    #[test]
    fn grid_layout_makes_tiles_as_large_as_possible() {
        let wide = Size::new(2000.0, 600.0);
        assert_eq!(layout(2, 4.0 / 3.0, wide).0, 2);
        assert_eq!(layout(4, 4.0 / 3.0, wide).0, 4);
        let tall = Size::new(600.0, 1600.0);
        assert_eq!(layout(2, 4.0 / 3.0, tall).0, 1);
        let square = Size::new(1000.0, 1000.0);
        let (columns, tile) = layout(4, 1.0, square);
        assert_eq!(columns, 2);
        assert_eq!(tile, Size::new(496.0, 496.0));
        assert_eq!(layout(1, 2.0, square).1, Size::new(1000.0, 500.0));
    }

    #[test]
    fn only_cameras_sharing_a_model_show_their_serial() {
        let mut cameras = vec![
            streaming_camera(0, 0, 30.0),
            streaming_camera(0, 0, 30.0),
            streaming_camera(0, 0, 30.0),
        ];
        cameras[2].info.model = "Line scan".into();
        assert_eq!(twins(&cameras), [true, true, false]);
        assert_eq!(twins(&cameras[1..]), [false, false]);
    }
}
