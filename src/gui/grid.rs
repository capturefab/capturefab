//! The multi-camera overview: every camera's image as large as the window
//! allows, captioned over the picture, with actions on hover.
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
/// How far a tile's badges sit in from its edges.
const INSET: f32 = 10.0;
/// How far a tile's actions sit in from its corner.
const ACTIONS_INSET: f32 = 8.0;
/// What a tile's actions take from its right edge on hover: three 24 px
/// buttons 2 apart, on glass padded by 3, and their inset.
const ACTIONS: f32 = 3.0 * 24.0 + 2.0 * 2.0 + 2.0 * 3.0 + ACTIONS_INSET;

/// Columns and tile size for `count` images of `aspect` in `area`: as
/// large as possible (see `layout`), unless that leaves tiles shorter than
/// `MIN_TILE_HEIGHT`. Then as many columns of that height as fit the width,
/// widened to fill it, and the grid scrolls.
fn tiles(count: usize, aspect: f32, area: Size) -> (usize, Size) {
    let (columns, tile) = layout(count, aspect, area);
    if tile.height >= MIN_TILE_HEIGHT {
        return (columns, tile);
    }
    let fit = (area.width + GAP) / (MIN_TILE_HEIGHT * aspect + GAP);
    let columns = (fit as usize).clamp(1, count.max(1));
    let width = ((area.width - GAP * (columns - 1) as f32) / columns as f32).max(1.0);
    (columns, Size::new(width, width / aspect))
}

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
        let (columns, tile) = tiles(cameras.len(), aspect, size);
        let rows = cameras.len().div_ceil(columns).max(1);
        let height = rows as f32 * tile.height + GAP * (rows - 1) as f32;
        let mut grid = column![].spacing(GAP).align_x(Alignment::Center);
        for chunk in cameras.chunks(columns) {
            let mut line = row![].spacing(GAP);
            for camera in chunk {
                line = line.push(self.tile(camera, tile));
            }
            grid = grid.push(line);
        }
        // As tall as the rows, so those that do not fit scroll into view.
        scrollable(
            container(grid)
                .center_x(size.width)
                .center_y(size.height.max(height)),
        )
        .style(style::scroll)
        .into()
    }

    /// A camera's picture with its caption, badges and, under the pointer,
    /// its actions.
    fn tile<'a>(&'a self, camera: &'a CameraSnapshot, size: Size) -> Element<'a, Message> {
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
        let mut layers = stack![
            picture,
            iced::widget::canvas(Corners {
                radius: style::RADIUS_MEDIUM
            })
            .width(Fill)
            .height(Fill),
            container(
                container(self.caption(camera, size.width))
                    .padding([10, 12])
                    .style(style::caption)
            )
            .align_bottom(Fill)
            .width(Fill),
        ];
        // The badges take the tile's width; the error's controls also keep
        // clear of the actions, and a gap.
        let stopped = !camera.streaming && preview.is_some_and(|preview| preview.shown.is_some());
        layers = layers.push(self.badges(
            id,
            stopped,
            size.width - 2.0 * INSET,
            ACTIONS - INSET + GAP,
            INSET,
        ));
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
                                    "Open · {} · worker PID {}",
                                    identity(&camera.info),
                                    camera.worker_pid
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
                .padding(ACTIONS_INSET),
            );
        }
        let flash = self.flash(id, false);
        if flash > 0.0 {
            layers = layers.push(
                container(space().width(Fill).height(Fill)).style(style::capture_flash(flash)),
            );
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
    /// which camera it is (with what tells it apart from another of its
    /// model), what else it does, and its frame rate.
    fn caption<'a>(&'a self, camera: &'a CameraSnapshot, width: f32) -> Element<'a, Message> {
        let stage = style::STAGE;
        let state = self.camera_state(camera);
        let twin = twin_identity(&self.snapshot.cameras, &camera.info);
        let model = one_line(
            camera.info.model.as_str(),
            style::BODY,
            style::SEMIBOLD,
            stage.text,
        );
        // For twins the identity keeps its room first, so the model they
        // share is what gets cut.
        let mut name = row![if twin.is_some() {
            model.yield_room()
        } else {
            model
        }]
        .spacing(7)
        .align_y(Alignment::Center);
        if let Some(identity) = twin {
            name = name.push(one_line(
                identity,
                style::SMALL,
                style::SANS,
                stage.secondary,
            ));
        }
        let mut caption = row![
            // The same state dot as the camera's row in the camera list.
            status_dot(state, 7.0, stage),
            // Fluid, so the marks and figures after it are laid out first.
            container(name).width(Fill),
        ]
        .spacing(7)
        .align_y(Alignment::Center);
        if let Some(output) = camera.forwarding.as_deref().map(Output::of) {
            caption = caption.push(chip(
                if output.recording() {
                    dot(stage.danger, 7.0)
                } else {
                    icon(Icon::Broadcast, 12.0, stage.accent_text)
                },
                output.about(),
            ));
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
            // In the dot's color: amber once frames stop, as on the pill.
            caption = caption.push(fps_spark(
                self.throughput.get(&camera.info.id),
                self.rate_coming(&camera.info.id),
                state.color(stage),
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
    fn tiles_too_short_take_fewer_columns_that_fill_the_width() {
        let aspect = 4.0 / 3.0;
        // Three columns would be 128 tall; two fill the width, and scroll.
        let (columns, tile) = tiles(9, aspect, Size::new(528.0, 597.0));
        assert_eq!(columns, 2);
        assert_eq!(tile, Size::new(260.0, 195.0));
        // Tall enough: as large as possible, as before.
        let area = Size::new(1000.0, 1000.0);
        assert_eq!(tiles(4, aspect, area), layout(4, aspect, area));
        for count in 1..=16 {
            for width in [100.0, 187.0, 400.0, 528.0, 900.0] {
                let area = Size::new(width, 300.0);
                let (columns, tile) = tiles(count, aspect, area);
                let row = columns as f32 * tile.width + GAP * (columns - 1) as f32;
                assert!(row <= width + 0.01, "{count} in {width}: {row}");
                assert!(columns >= 1 && columns <= count);
            }
        }
    }
}
