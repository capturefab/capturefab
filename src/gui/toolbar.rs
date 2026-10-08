//! The bottom bar and the activity log above it.
use super::*;
use iced::widget::column;

/// The widest the status at the right of the toolbar grows.
const STATUS: f32 = 420.0;
/// Room the Capture button and the output name keep at the left.
const CAPTURE_ROOM: f32 = 230.0;
/// Room a notice's glyph and the gap after it take.
const NOTICE_GLYPH: f32 = 13.0 + 6.0;
/// Room a notice's "Details" link and the gap before it take.
const NOTICE_DETAILS: f32 = 52.0 + 4.0;
/// Room a notice's dismiss button and the gap before it take.
const NOTICE_DISMISS: f32 = 21.0 + 2.0;

/// `style` with its text and fill faded by `alpha`, for controls that come
/// and go with a notice.
fn faded(
    style: fn(&Theme, button::Status) -> button::Style,
    alpha: f32,
) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |theme, status| {
        let mut style = style(theme, status);
        style.text_color = fade(style.text_color, alpha);
        if let Some(iced::Background::Color(color)) = style.background {
            style.background = Some(fade(color, alpha).into());
        }
        style
    }
}

impl Workbench {
    pub(super) fn toolbar(&self, p: &'static Palette) -> Element<'_, Message> {
        let snapshot = &self.snapshot;
        let os = Os::CURRENT;
        // Nothing to capture from until a camera connects.
        let capture = snapshot.connected.is_some() || self.overview();
        let width = self.status_width(capture);
        let mut left = row![].spacing(8).align_y(Alignment::Center);
        if capture {
            left = left.push(self.capture_button(p)).push(tip_above(
                button(clipped(
                    text(self.capture_to.label(&self.output))
                        .size(style::SMALL)
                        .color(p.secondary)
                        .wrapping(text::Wrapping::None),
                ))
                .padding([4, 6])
                .style(style::plain)
                .on_press(Message::ShowTab(Tab::Capture)),
                Action::CaptureTab.hint("Where captures are saved", os),
            ));
        }
        let unseen = self.chrome.unseen.filter(|_| !self.logs_open);
        let mut activity = stack![tip_above(
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
            Action::ToggleActivity.hint(
                match unseen {
                    Some(Level::Error) => "Session activity: new errors",
                    Some(_) => "Session activity: new warnings",
                    None => "Session activity",
                },
                os
            ),
        )];
        if let Some(level) = unseen {
            // A mark over the glyph's corner; it takes no input.
            activity = activity.push(
                container(dot(
                    if level == Level::Error {
                        p.danger
                    } else {
                        p.warn
                    },
                    6.0,
                ))
                .align_right(Fill)
                .padding([5, 5]),
            );
        }
        column![
            rule::horizontal(1).style(style::line),
            container(
                row![
                    container(left).width(Fill).clip(true),
                    container(self.status(width, p)).max_width(width).clip(true),
                    activity,
                ]
                .spacing(10)
                .align_y(Alignment::Center),
            )
            .height(TOOLBAR - 1.0)
            .align_y(Alignment::Center)
            .padding([0.0, GUTTER - 6.0])
            .clip(true),
        ]
        .into()
    }

    /// How wide the status may grow: `STATUS`, less what a narrow main area
    /// must keep for the capture controls when they show.
    fn status_width(&self, capture: bool) -> f32 {
        // Padding, gaps and the Activity button.
        let mut room = self.main_width() - 2.0 * (GUTTER - 6.0) - 2.0 * 10.0 - 28.0;
        if capture {
            room -= CAPTURE_ROOM;
        }
        room.clamp(160.0, STATUS)
    }

    /// Capture for the selected camera. While saving, the spinner takes the
    /// glyph's place; just after, it reads as saved.
    fn capture_button(&self, p: &'static Palette) -> Element<'_, Message> {
        let os = Os::CURRENT;
        let connected = self.snapshot.connected.is_some();
        let capturing = self.pending("Saving capture");
        // One capture runs at a time, from any camera; saved is per camera.
        let saved = !capturing && self.just_saved(self.snapshot.active_camera.as_deref());
        let (glyph, label): (Element<'_, Message>, _) = if capturing {
            (icon::spinner(14.0, p.secondary, self.spin()), "Saving…")
        } else if saved {
            (icon(Icon::Check, 14.0, p.live), "Saved")
        } else {
            (
                icon(
                    Icon::Camera,
                    14.0,
                    if connected { p.text } else { p.tertiary },
                ),
                if self.overview() {
                    "Capture selected"
                } else {
                    "Capture"
                },
            )
        };
        tip_above(
            button(
                row![glyph, text(label).size(style::BODY)]
                    .spacing(7)
                    .align_y(Alignment::Center),
            )
            .padding([6, 12])
            // Ignoring presses while saving would otherwise read as disabled.
            .style(move |theme, status| {
                let status = if capturing {
                    button::Status::Active
                } else {
                    status
                };
                style::secondary(theme, status)
            })
            .on_press_maybe((connected && !capturing).then_some(Message::Capture)),
            if capturing {
                "Saving the capture…".to_owned()
            } else if !connected {
                "Select a camera to capture from".to_owned()
            } else {
                Action::Capture.hint("Save using the settings in Capture", os)
            },
        )
    }

    /// The toolbar's status: the notice, else the newest command on its
    /// way, else the camera's link.
    fn status(&self, width: f32, p: &'static Palette) -> Element<'_, Message> {
        let snapshot = &self.snapshot;
        if let Some(notice) = &self.notice {
            return self.notice_line(notice, width, p);
        }
        if let Some(pending) = self.pending.iter().max_by_key(|pending| pending.at) {
            // The newest command, most likely the one just asked for.
            return row![
                dot(fade(p.accent, self.pulse()), 6.0),
                one_line(
                    format!("{}…", pending.label),
                    style::SMALL,
                    style::SANS,
                    p.secondary
                ),
            ]
            .spacing(7)
            .align_y(Alignment::Center)
            .into();
        }
        let Some(camera) = &snapshot.connected else {
            return space().into();
        };
        let mut parts = row![
            text(format!("{} · {}", camera.transport, camera.serial))
                .size(style::SMALL)
                .color(p.secondary)
                .wrapping(text::Wrapping::None)
        ]
        .spacing(10);
        if let Some(stats) = &snapshot.transport {
            parts = parts.push(tip_above(
                text(transport_text(stats))
                    .size(style::SMALL)
                    .color(if stats.notes.is_empty() {
                        p.secondary
                    } else {
                        p.ink(p.warn)
                    })
                    .wrapping(text::Wrapping::None),
                if stats.notes.is_empty() {
                    "Packet size · resent packets recovered/requested · incomplete or missing frames"
                        .into()
                } else {
                    stats.notes.join("\n")
                },
            ));
        }
        parts.into()
    }

    /// A notice on one line: its glyph and text, with a way to the whole
    /// story in the activity log for warnings and errors, and a way to put
    /// an error away.
    fn notice_line<'a>(
        &self,
        notice: &'a Notice,
        width: f32,
        p: &'static Palette,
    ) -> Element<'a, Message> {
        let alpha = self.notice_alpha();
        let (glyph, tint, ink) = match notice.level {
            Level::Done => (Icon::Check, p.live, p.secondary),
            Level::Warning => (Icon::WarningTriangle, p.warn, p.secondary),
            Level::Error => (Icon::Warning, p.danger, p.ink(p.danger)),
        };
        let details = notice.level != Level::Done && !self.logs_open;
        let dismiss = notice.level == Level::Error;
        let mut room = width - NOTICE_GLYPH;
        if details {
            room -= NOTICE_DETAILS;
        }
        if dismiss {
            room -= NOTICE_DISMISS;
        }
        let mut line = row![
            icon(glyph, 13.0, fade(tint, alpha)),
            space().width(6),
            tip_above(
                container(one_line(
                    notice.text.as_str(),
                    style::SMALL,
                    style::SANS,
                    fade(ink, alpha),
                ))
                .max_width(room),
                notice.about(),
            ),
        ]
        .align_y(Alignment::Center);
        if details {
            line = line.push(space().width(4)).push(
                button(text("Details").size(style::SMALL))
                    .padding([2, 6])
                    .style(faded(style::link, alpha))
                    .on_press(Message::ShowActivity),
            );
        }
        if dismiss {
            line = line.push(space().width(2)).push(tip_above(
                button(icon(Icon::Close, 11.0, fade(p.secondary, alpha)))
                    .padding(4)
                    .style(faded(style::plain, alpha))
                    .on_press(Message::DismissNotice),
                "Dismiss",
            ));
        }
        line.into()
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
        let (warn, danger) = (p.ink(p.warn), p.ink(p.danger));
        for entry in &self.snapshot.logs {
            // Errors and warnings read by shape as well as color, as notices do.
            let (mark, ink): (Element<'_, Message>, _) =
                if entry.level.eq_ignore_ascii_case("error") {
                    (icon(Icon::Warning, 11.0, p.danger), danger)
                } else if entry.level.eq_ignore_ascii_case("warn")
                    || entry.level.eq_ignore_ascii_case("warning")
                {
                    (icon(Icon::WarningTriangle, 11.0, p.warn), warn)
                } else {
                    (space().into(), p.text)
                };
            lines = lines.push(
                row![
                    // Failed commands are logged without a time.
                    text(if entry.time.is_empty() {
                        "—"
                    } else {
                        entry.time.as_str()
                    })
                    .size(style::CAPTION)
                    .font(style::MONO)
                    .color(p.secondary)
                    .width(Length::Fixed(58.0)),
                    container(mark).width(Length::Fixed(11.0)),
                    text(entry.message.as_str())
                        .size(style::CAPTION)
                        .font(style::MONO)
                        .color(ink),
                ]
                .spacing(6)
                .align_y(Alignment::Center),
            );
        }
        column![
            rule::horizontal(1).style(style::line),
            container(
                column![
                    row![
                        text("Activity").size(style::BODY).font(style::SEMIBOLD),
                        space::horizontal(),
                        button(
                            text(if self.just_copied(notice::COPIED_LOG) {
                                "Copied"
                            } else {
                                "Copy log"
                            })
                            .size(style::SMALL)
                        )
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
