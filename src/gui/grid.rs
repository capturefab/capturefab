//! The multi-camera overview grid.
use super::*;
use iced::widget::column;

impl Workbench {
    pub(super) fn overview_view(&self, p: &'static Palette) -> Element<'_, Message> {
        let snapshot = &self.snapshot;
        let streaming = snapshot.cameras.iter().filter(|c| c.streaming).count();
        let subtitle = text(format!(
            "{} connected · {streaming} streaming",
            snapshot.cameras.len()
        ))
        .size(style::BODY)
        .color(p.secondary);
        let start = snapshot.cameras.iter().any(|camera| !camera.streaming);
        let manual = snapshot.cameras.iter().any(|camera| camera.auto.is_none());
        let actions = row![
            button(text(if manual { "Auto all" } else { "Manual all" }).size(style::BODY))
                .padding([7, 12])
                .style(style::secondary)
                .on_press(Message::AllAuto(manual)),
            stream_button(
                !start,
                if start { "Start all" } else { "Stop all" },
                Some(Message::AllStreams(start)),
                p,
            )
            .width(Length::Shrink),
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        let header = self.header(
            icon(Icon::Grid, 24.0, p.accent),
            "All Cameras".into(),
            subtitle.into(),
            actions.into(),
        );
        let grid = responsive(move |size| self.grid(size, p));
        column![
            header,
            space().height(18),
            container(grid).width(Fill).height(Fill),
            space().height(6),
            text(format!(
                "Click a preview to inspect its settings · {} / {} switches camera · {} focuses",
                Action::PreviousCamera.shortcut(Os::CURRENT),
                Action::NextCamera.shortcut(Os::CURRENT),
                Action::FocusCamera.shortcut(Os::CURRENT),
            ))
            .size(style::CAPTION)
            .color(p.tertiary),
            space().height(12),
        ]
        .padding(iced::Padding {
            top: 0.0,
            right: GUTTER,
            bottom: 0.0,
            left: GUTTER,
        })
        .height(Fill)
        .into()
    }

    pub(super) fn grid(&self, size: Size, p: &'static Palette) -> Element<'_, Message> {
        let cameras = &self.snapshot.cameras;
        let count = cameras.len();
        let gap = 14.0;
        let columns = if count == 2 {
            if size.width >= 460.0 { 2 } else { 1 }
        } else {
            ((size.width / 240.0).floor() as usize)
                .max(1)
                .min((count as f32).sqrt().ceil() as usize)
        };
        let rows = count.div_ceil(columns);
        let tile_height = ((size.height - gap * (rows - 1) as f32) / rows as f32).max(230.0);
        let mut grid = column![].spacing(gap);
        for chunk in cameras.chunks(columns) {
            let mut line = row![].spacing(gap).height(tile_height);
            for camera in chunk {
                line = line.push(self.tile(camera, p));
            }
            for _ in chunk.len()..columns {
                line = line.push(space().width(Fill));
            }
            grid = grid.push(line);
        }
        scrollable(grid).style(style::scroll).into()
    }

    pub(super) fn tile<'a>(
        &'a self,
        camera: &'a crate::session::CameraSnapshot,
        p: &'static Palette,
    ) -> Element<'a, Message> {
        let id = &camera.info.id;
        let active = self.snapshot.active_camera.as_ref() == Some(id);
        let preview = self.previews.get(id);
        let picture: Element<'_, Message> = match preview.and_then(|preview| preview.shown.as_ref())
        {
            Some(shown) => {
                let mut layers = stack![shown.view(&self.gpu, None)];
                if !camera.streaming {
                    layers = layers.push(container(last_frame()).padding(9));
                }
                layers.into()
            }
            None => center(
                text(if camera.streaming {
                    "Waiting for a frame…"
                } else {
                    "Ready to stream"
                })
                .size(style::BODY)
                .color(p.secondary),
            )
            .into(),
        };
        let picture = tip(
            button(
                container(picture)
                    .width(Fill)
                    .height(Fill)
                    .clip(true)
                    .style(style::stage),
            )
            .padding(0)
            .width(Fill)
            .height(Fill)
            .style(style::bare)
            .on_press(Message::Select(id.clone())),
            format!(
                "Select {} for settings and capture\nAcquisition worker PID {}",
                camera.info.serial, camera.worker_pid
            ),
        );
        let mut title = row![
            dot(
                if camera.streaming {
                    fade(p.live, self.pulse())
                } else {
                    p.tertiary
                },
                8.0
            ),
            clipped(
                text(camera.info.model.clone())
                    .size(14)
                    .font(style::SEMIBOLD)
            ),
            space::horizontal(),
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        if active {
            title = title.push(
                text("Selected")
                    .size(style::CAPTION)
                    .font(style::MEDIUM)
                    .color(p.accent_text),
            );
        }
        let mut stats = row![
            text(format!("{:.1} fps", camera.fps))
                .size(style::SMALL)
                .font(style::MONO)
                .color(if camera.streaming {
                    p.text
                } else {
                    p.secondary
                }),
            text(format!("{} frames", grouped(camera.frames)))
                .size(style::SMALL)
                .color(p.secondary),
        ]
        .spacing(10)
        .align_y(Alignment::Center);
        if let Some(destination) = &camera.forwarding {
            stats = stats.push(tip(
                text("Out")
                    .size(style::CAPTION)
                    .font(style::SEMIBOLD)
                    .color(p.accent_text),
                format!("Forwarding to {}", redact_address(destination)),
            ));
        }
        if let Some(auto) = &camera.auto {
            stats = stats.push(tip(
                text("Auto")
                    .size(style::CAPTION)
                    .font(style::SEMIBOLD)
                    .color(p.accent_text),
                format!("Auto mode · {} · balance {:.2}", auto.state, auto.balance),
            ));
        }
        let lost = camera
            .transport
            .as_ref()
            .map_or(camera.dropped, transport_loss);
        if lost > 0 {
            stats = stats.push(
                text(format!("{lost} lost"))
                    .size(style::SMALL)
                    .color(p.warn),
            );
        }
        let mut controls = row![
            button(text(if camera.streaming { "Stop" } else { "Start" }).size(style::SMALL))
                .padding([3, 8])
                .style(style::link)
                .on_press(Message::StreamCamera(id.clone(), !camera.streaming)),
        ]
        .spacing(4)
        .align_y(Alignment::Center);
        if !active {
            controls = controls.push(
                button(text("Select").size(style::SMALL))
                    .padding([3, 8])
                    .style(style::plain)
                    .on_press(Message::Select(id.clone())),
            );
        }
        controls = controls.push(space::horizontal());
        if let Some((_, width, height, format)) = preview.and_then(|preview| preview.meta) {
            controls = controls.push(
                text(format!("{width}×{height} {}", pixel_name(format)))
                    .size(style::CAPTION)
                    .color(p.tertiary),
            );
        }
        let mut body = column![picture, title, stats, controls].spacing(8);
        if let Some(error) = preview
            .and_then(|preview| preview.error.as_ref())
            .or(camera.last_error.as_ref())
        {
            body = body.push(tip(
                clipped(text(error.clone()).size(style::CAPTION).color(p.danger)),
                error.clone(),
            ));
        }
        container(body)
            .padding(10)
            .width(Fill)
            .height(Fill)
            .style(style::tile(active))
            .into()
    }
}
