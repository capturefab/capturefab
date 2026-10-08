//! The bottom bar and the activity log above it.
use super::*;
use iced::widget::column;

impl Workbench {
    pub(super) fn toolbar(&self, p: &'static Palette) -> Element<'_, Message> {
        let snapshot = &self.snapshot;
        let connected = snapshot.connected.is_some();
        let capturing = self.pending("Saving capture");
        let os = Os::CURRENT;
        let mut left = row![tip(
            button(
                row![
                    icon(
                        Icon::Camera,
                        14.0,
                        if connected { p.text } else { p.tertiary }
                    ),
                    text(if capturing {
                        "Saving…"
                    } else if self.overview() {
                        "Capture selected"
                    } else {
                        "Capture"
                    })
                    .size(style::BODY),
                ]
                .spacing(7)
                .align_y(Alignment::Center),
            )
            .padding([6, 12])
            .style(style::secondary)
            .on_press_maybe((connected && !capturing).then_some(Message::Capture)),
            Action::Capture.hint("Save using the settings in Capture", os),
        ),]
        .spacing(8)
        .align_y(Alignment::Center);
        left = left.push(tip(
            button(clipped(
                text(self.capture_to.label(&self.output))
                    .size(style::SMALL)
                    .color(p.secondary)
                    .wrapping(text::Wrapping::None),
            ))
            .padding([4, 6])
            .style(style::plain)
            .on_press(Message::Tab(Tab::Capture)),
            Action::CaptureTab.hint("Where captures are saved", os),
        ));
        let status: Element<'_, Message> = if let Some(notice) = &self.notice {
            let alpha = self.notice_alpha();
            let (glyph, tint, ink) = match notice.level {
                Level::Done => (Icon::Check, p.live, p.secondary),
                Level::Warning => (Icon::Warning, p.warn, p.secondary),
                Level::Error => (Icon::Warning, p.danger, p.danger),
            };
            tip(
                row![
                    icon(glyph, 13.0, fade(tint, alpha)),
                    clipped(
                        text(notice.text.clone())
                            .size(style::SMALL)
                            .color(fade(ink, alpha))
                            .wrapping(text::Wrapping::None)
                    ),
                ]
                .spacing(6)
                .align_y(Alignment::Center),
                notice.about(),
            )
        } else if let Some(pending) = self.pending.first() {
            row![
                dot(fade(p.accent, self.pulse()), 6.0),
                text(format!("{}…", pending.label))
                    .size(style::SMALL)
                    .color(p.secondary)
            ]
            .spacing(7)
            .align_y(Alignment::Center)
            .into()
        } else if let Some(camera) = &snapshot.connected {
            let mut parts = row![
                text(format!("{} · {}", camera.transport, camera.serial))
                    .size(style::SMALL)
                    .color(p.tertiary)
            ]
            .spacing(10);
            if let Some(stats) = &snapshot.transport {
                parts = parts.push(tip(
                    text(transport_text(stats)).size(style::SMALL).color(
                        if stats.notes.is_empty() {
                            p.tertiary
                        } else {
                            p.warn
                        },
                    ),
                    if stats.notes.is_empty() {
                        "Packet size · resent packets recovered/requested · incomplete or missing frames".into()
                    } else {
                        stats.notes.join("\n")
                    },
                ));
            }
            parts.into()
        } else {
            text("Ready").size(style::SMALL).color(p.tertiary).into()
        };
        column![
            rule::horizontal(1).style(style::line),
            container(
                row![
                    container(left).width(Fill).clip(true),
                    container(status).max_width(420).clip(true),
                    tip(
                        icon_button(
                            Icon::Activity,
                            16.0,
                            if self.logs_open {
                                p.accent
                            } else {
                                p.secondary
                            },
                            Message::ToggleActivity,
                        ),
                        Action::ToggleActivity.hint("Session activity", os),
                    ),
                ]
                .spacing(10)
                .align_y(Alignment::Center),
            )
            .height(TOOLBAR - 1.0)
            .align_y(Alignment::Center)
            .padding([0.0, GUTTER - 6.0]),
        ]
        .into()
    }

    pub(super) fn activity(&self, p: &'static Palette) -> Element<'_, Message> {
        let mut lines = column![].spacing(3);
        if self.snapshot.logs.is_empty() {
            lines = lines.push(
                text("Session events will appear here.")
                    .size(style::SMALL)
                    .color(p.secondary),
            );
        }
        for entry in &self.snapshot.logs {
            lines = lines.push(
                row![
                    text(entry.time.clone())
                        .size(style::CAPTION)
                        .font(style::MONO)
                        .color(p.tertiary),
                    text(entry.message.clone())
                        .size(style::CAPTION)
                        .font(style::MONO)
                        .color(if entry.level.eq_ignore_ascii_case("error") {
                            p.danger
                        } else {
                            p.secondary
                        }),
                ]
                .spacing(12),
            );
        }
        column![
            rule::horizontal(1).style(style::line),
            container(
                column![
                    row![
                        text("Activity").size(style::BODY).font(style::SEMIBOLD),
                        space::horizontal(),
                        button(text("Copy log").size(style::SMALL))
                            .padding([3, 8])
                            .style(style::link)
                            .on_press(Message::CopyLog),
                    ]
                    .align_y(Alignment::Center),
                    scrollable(lines)
                        .anchor_bottom()
                        .width(Fill)
                        .height(Fill)
                        .style(style::scroll),
                ]
                .spacing(6),
            )
            .height(170)
            .padding([10.0, GUTTER]),
        ]
        .into()
    }
}
