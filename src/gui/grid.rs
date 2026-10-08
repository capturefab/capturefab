//! The multi-camera overview: every camera's image as large as the window
//! allows, captioned over the picture, with actions on hover.
use super::titlebar::{fps_spark, status_pill};
use super::*;
use iced::widget::column;

const GAP: f32 = 8.0;
const MIN_TILE_HEIGHT: f32 = 140.0;

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
        let snapshot = &self.snapshot;
        let streaming = snapshot.cameras.iter().filter(|c| c.streaming).count();
        let status = status_pill(
            if streaming > 0 { "Streaming" } else { "Ready" },
            Some(format!("{streaming} of {}", snapshot.cameras.len())),
            None,
            if streaming > 0 { p.live } else { p.accent_text },
            if streaming > 0 { self.pulse() } else { 1.0 },
        );
        let start = snapshot.cameras.iter().any(|camera| !camera.streaming);
        let manual = snapshot.cameras.iter().any(|camera| camera.auto.is_none());
        let actions = row![
            button(
                text(if manual { "Auto all" } else { "Manual all" })
                    .size(style::BODY)
                    .font(style::MEDIUM)
            )
            .padding([5, 12])
            .style(style::toggle(!manual))
            .on_press(Message::AllAuto(manual)),
            stream_button(
                !start,
                if start { "Start all" } else { "Stop all" },
                Some(Message::AllStreams(start)),
                p,
            ),
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        let header = self.title_bar("All cameras".into(), Some(status), actions.into(), p);
        let grid = responsive(move |size| self.grid(size, p));
        column![
            header,
            container(grid)
                .padding(GAP)
                .width(Fill)
                .height(Fill)
                .style(style::stage),
        ]
        .into()
    }

    fn grid(&self, size: Size, p: &'static Palette) -> Element<'_, Message> {
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
        let mut grid = column![].spacing(GAP).align_x(Alignment::Center);
        for chunk in cameras.chunks(columns) {
            let mut line = row![].spacing(GAP);
            for camera in chunk {
                line = line.push(self.tile(camera, tile, p));
            }
            grid = grid.push(line);
        }
        scrollable(container(grid).center_x(size.width).center_y(size.height))
            .style(style::scroll)
            .into()
    }

    fn tile<'a>(
        &'a self,
        camera: &'a crate::session::CameraSnapshot,
        size: Size,
        p: &'static Palette,
    ) -> Element<'a, Message> {
        let id = &camera.info.id;
        let active = self.snapshot.active_camera.as_ref() == Some(id);
        let hovered = self.hovered_tile.as_ref() == Some(id);
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
                .color(style::ON_STAGE_SECONDARY),
            )
            .into(),
        };
        let ring = if active { self.ring.get(self.now) } else { 0.0 };
        let picture = tip(
            button(
                container(picture)
                    .width(Fill)
                    .height(Fill)
                    .clip(true)
                    .style(style::tile),
            )
            .padding(0)
            .width(Fill)
            .height(Fill)
            .style(style::bare)
            .on_press(Message::Select(id.clone())),
            format!(
                "{} · S/N {}\nClick to select, double-click to open\nAcquisition worker PID {}",
                camera.info.model, camera.info.serial, camera.worker_pid
            ),
        );
        let lost = camera
            .transport
            .as_ref()
            .map_or(camera.dropped, transport_loss);
        let mut caption = row![
            dot(
                if camera.streaming {
                    fade(style::DARK.live, self.pulse())
                } else {
                    style::ON_STAGE_SECONDARY
                },
                7.0
            ),
            clipped(
                text(camera.info.model.clone())
                    .size(style::BODY)
                    .font(style::SEMIBOLD)
                    .wrapping(text::Wrapping::None)
            ),
            space::horizontal(),
        ]
        .spacing(7)
        .align_y(Alignment::Center);
        if camera.forwarding.is_some() {
            caption = caption.push(
                text("Out")
                    .size(style::CAPTION)
                    .font(style::SEMIBOLD)
                    .color(p.accent),
            );
        }
        if camera.auto.is_some() {
            caption = caption.push(
                text("Auto")
                    .size(style::CAPTION)
                    .font(style::SEMIBOLD)
                    .color(p.accent),
            );
        }
        if lost > 0 {
            caption = caption.push(
                text(format!("{lost} lost"))
                    .size(style::SMALL)
                    .color(style::DARK.warn),
            );
        }
        if camera.streaming {
            caption = caption.push(fps_spark(
                self.throughput.get(id),
                style::DARK.live,
                style::DARK.warn,
                (52.0, 14.0),
            ));
        }
        caption = caption.push(
            text(format!("{:.1} fps", camera.fps))
                .size(style::SMALL)
                .color(style::ON_STAGE_SECONDARY)
                .width(58)
                .align_x(Alignment::End),
        );
        let mut layers = stack![
            picture,
            container(container(caption).padding([10, 12]).style(style::caption))
                .align_bottom(Fill)
                .width(Fill),
        ];
        let mut badges = row![].spacing(6);
        if !camera.streaming && preview.is_some_and(|preview| preview.shown.is_some()) {
            badges = badges.push(last_frame());
        }
        if let Some(error) = preview
            .and_then(|preview| preview.error.as_ref())
            .or(camera.last_error.as_ref())
        {
            badges = badges.push(tip(
                container(icon(Icon::Warning, 12.0, style::DARK.danger))
                    .padding([3, 6])
                    .style(style::badge),
                error.clone(),
            ));
        }
        layers = layers.push(container(badges).padding(10));
        if hovered {
            let shown = self.tile_hover.get(self.now);
            let glass = |kind: Icon, hint: &'static str, on: Message| {
                tip(
                    button(icon(kind, 14.0, fade(style::ON_STAGE, shown)))
                        .padding(5)
                        .style(style::on_glass(false, shown))
                        .on_press(on),
                    hint,
                )
            };
            layers = layers.push(
                container(
                    container(
                        row![
                            glass(
                                if camera.streaming {
                                    Icon::Stop
                                } else {
                                    Icon::Play
                                },
                                if camera.streaming {
                                    "Stop stream"
                                } else {
                                    "Start stream"
                                },
                                Message::StreamCamera(id.clone(), !camera.streaming),
                            ),
                            glass(
                                Icon::Camera,
                                "Capture with the settings in Capture",
                                Message::CaptureCamera(id.clone()),
                            ),
                            glass(
                                Icon::CornersOut,
                                "Open this camera",
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
}
