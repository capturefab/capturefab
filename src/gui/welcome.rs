//! What the stage shows before pictures: the welcome screen with no camera
//! connected, and a connected camera before its first frame.
use super::*;
use iced::widget::column;
use sidebar::{ADDRESSES, DISCOVERING, DISCOVERS, ISSUES, place, recent_place};

/// Height of a welcome card: its padding and two lines of text.
const CARD: f32 = 10.0 + 13.0 * 1.3 + 2.0 + 11.0 * 1.3 + 10.0;
/// Height of the discovery callout.
const CALLOUT: f32 = 4.0 + 12.0 * 1.3 + 4.0;
/// Height of the welcome screen's content with one card and nothing else,
/// which sits centered; more grows downward from there, so nothing above
/// the cards ever moves as discovery changes them.
const ANCHORED: f32 = 280.0;
/// How far the welcome screen rises as it fades in.
const RISE: f32 = 24.0;

impl Workbench {
    /// A connected camera before its first frame. On the first connect the
    /// surface dims from the welcome screen's to the stage's.
    pub(super) fn ready(&self, p: &'static Palette) -> Element<'_, Message> {
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
                    stream_button(
                        false,
                        "Start stream",
                        Some(Message::ToggleStream),
                        self.snapshot
                            .active_camera
                            .as_deref()
                            .and_then(|id| self.stream_pending(id))
                            .is_some(),
                        self.spin(),
                        p,
                    )
                    .padding([7, 16]),
                    button(text("Capture one frame").size(style::BODY))
                        .padding([7, 12])
                        .style(style::on_glass(false, 1.0))
                        .on_press_maybe((!self.pending(Job::Capture)).then_some(Message::Capture)),
                ]
                .spacing(10)
                .align_y(Alignment::Center),
            );
        }
        let t = self.side.stage_in.get(self.now);
        let surface = style::mix(p.base, p.stage, t);
        let ready = center(ready).padding(iced::Padding {
            top: motion::rise(16.0, t),
            ..iced::Padding::ZERO
        });
        container(veil(ready, surface, 0.0, t))
            .width(Fill)
            .height(Fill)
            .style(move |_| container::Style {
                background: Some(surface.into()),
                text_color: Some(style::ON_STAGE),
                ..container::Style::default()
            })
            .into()
    }

    pub(super) fn welcome(&self, p: &'static Palette) -> Element<'_, Message> {
        let t = self.welcome.get(self.now);
        let block = responsive(move |size| {
            let (content, extra) = self.welcome_content(p);
            // Centered with one card; taller content grows downward until
            // it would run off the bottom, then rises just enough.
            let room = size.height - 2.0 * RISE;
            let top = ((size.height - ANCHORED) / 2.0)
                .min(room - ANCHORED - extra)
                .max(RISE);
            container(content)
                .center_x(Fill)
                .padding(iced::Padding {
                    top: top + motion::rise(RISE, t),
                    right: RISE,
                    bottom: 0.0,
                    left: RISE,
                })
                .into()
        });
        veil(container(block).clip(true), p.base, 0.0, t)
    }

    /// The welcome screen's column, and roughly how much taller than
    /// `ANCHORED` it is.
    pub(super) fn welcome_content(&self, p: &'static Palette) -> (Element<'_, Message>, f32) {
        let now = self.now;
        let discovering = self.pending(Job::Discover);
        let spin = self.spin();
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
        // A quick search keeps what the screen said; its button spins.
        let searching_shown = discovering && self.searching_shown();
        let subtitle = if !found.is_empty() {
            "Choose a camera to connect.".to_owned()
        } else if searching_shown {
            DISCOVERING.into()
        } else {
            "No cameras found. Try the simulator, or enter an address or stream URL.".into()
        };
        let mut extra = 0.0;
        let mut content = column![
            icon(Icon::Mark, 44.0, p.accent),
            space().height(10),
            text("Connect a camera")
                .size(style::DISPLAY)
                .font(style::BOLD)
                .color(p.text),
            text(subtitle)
                .size(style::BODY)
                .color(p.secondary)
                .align_x(Alignment::Center),
        ]
        .spacing(4)
        .align_x(Alignment::Center)
        .max_width(460);
        // The discovery callout's slot: empty without issues, so the
        // callout grows into it and only what follows moves.
        let mut issues: Element<'_, Message> = space().height(0).into();
        if let Some(issue) = self.side.discovery_issues.first() {
            let ink = Level::Warning.color(p);
            // As wide as the cards, in the warning tint of a status pill.
            let callout = tip(
                container(
                    row![
                        Level::Warning.mark(12.0, p),
                        one_line(first_line(issue), style::SMALL, style::SANS, ink).width(Fill),
                        button(text("Details").size(style::SMALL))
                            .style(style::link)
                            .padding([0, 6])
                            .on_press(Message::ShowActivity),
                    ]
                    .spacing(6)
                    .align_y(Alignment::Center),
                )
                .width(Fill)
                .padding(iced::Padding {
                    top: 4.0,
                    right: 4.0,
                    bottom: 4.0,
                    left: 10.0,
                })
                .style(move |theme| container::Style {
                    border: iced::border::rounded(style::RADIUS),
                    ..style::pill(p.warn)(theme)
                }),
                self.side.discovery_issues.join("\n"),
            );
            let arrival = self.side.arrival(ISSUES, now);
            let callout =
                container(veil(callout, p.base, style::RADIUS, arrival)).padding(iced::Padding {
                    top: 10.0,
                    ..iced::Padding::ZERO
                });
            issues = grow(callout.into(), CALLOUT + 10.0, arrival);
            extra += CALLOUT + 10.0;
        }
        content = content.push(issues).push(space().height(10));
        let mut cards = column![].spacing(8).width(Fill);
        if found.is_empty() {
            // One slot, the height of a card, that says what discovery is
            // doing or what to check; a card found takes it without a jump.
            cards = cards.push(if searching_shown {
                searching(spin, p)
            } else {
                not_found(p)
            });
        }
        // When every card is new, the first takes the slot and only fades.
        let all_new = found
            .iter()
            .all(|device| self.side.arrival(&device.id, now) < 1.0);
        for (index, device) in found.iter().enumerate() {
            let card = self.welcome_card(
                Icon::transport(device.transport),
                &device.model,
                place(device),
                &device.id,
                spin,
                p,
            );
            let arrival = self.side.arrival(&device.id, now);
            let card = veil(card, p.base, style::RADIUS_MEDIUM, arrival);
            cards = cards.push(if index == 0 && all_new {
                card
            } else {
                grow(card, CARD, arrival)
            });
        }
        if !recent.is_empty() {
            cards = cards.push(
                container(
                    text("Recent")
                        .size(style::SMALL)
                        .font(style::SEMIBOLD)
                        .color(p.secondary),
                )
                .padding(iced::Padding {
                    top: 8.0,
                    left: 2.0,
                    ..iced::Padding::ZERO
                }),
            );
            extra += 30.0;
        }
        for entry in &recent {
            cards = cards.push(self.welcome_card(
                Icon::transport(entry.transport),
                &entry.label,
                recent_place(entry),
                &entry.target,
                spin,
                p,
            ));
        }
        extra += (found.len() + recent.len()).saturating_sub(1) as f32 * (CARD + 8.0);
        let os = Os::CURRENT;
        let action = |kind: Icon, label: &'static str, on: Message, busy: bool, primary: bool| {
            let ink = if primary { Color::WHITE } else { p.secondary };
            let glyph = if busy {
                icon::spinner(14.0, ink, spin)
            } else {
                icon(kind, 15.0, ink)
            };
            let base = if primary {
                style::primary
            } else {
                style::secondary
            };
            button(
                row![glyph, text(label).size(style::BODY).font(style::MEDIUM)]
                    .spacing(7)
                    .align_y(Alignment::Center),
            )
            .padding([7, 12])
            // Busy controls ignore presses but keep their look.
            .style(move |theme, status| {
                base(theme, if busy { button::Status::Active } else { status })
            })
            .on_press_maybe((!busy).then_some(on))
        };
        let simulator_first = found.is_empty() && recent.is_empty() && !searching_shown;
        let actions = row![
            tip(
                action(
                    Icon::Refresh,
                    if discovering {
                        "Searching…"
                    } else {
                        "Search again"
                    },
                    Message::Discover,
                    discovering,
                    false,
                ),
                Action::Discover.hint(DISCOVERS, os),
            ),
            action(
                Icon::Cube,
                "Try a simulated camera",
                Message::TrySimulator,
                self.connecting("sim:0"),
                simulator_first,
            ),
            tip(
                action(
                    Icon::Plus,
                    "Enter an address",
                    Message::EnterAddress,
                    false,
                    false,
                ),
                Action::ConnectAddress.hint(ADDRESSES, os),
            ),
        ]
        .spacing(8)
        .wrap()
        .vertical_spacing(8);
        let guide = row![
            button(text("Quick guide").size(style::SMALL))
                .style(style::link)
                .padding([2, 6])
                .on_press(Message::Help(true)),
            text(Action::Help.key_label(os))
                .size(style::CAPTION)
                .color(p.secondary),
        ]
        .spacing(2)
        .align_y(Alignment::Center);
        let content = content
            .push(cards)
            .push(space().height(14))
            .push(actions)
            .push(space().height(14))
            .push(guide);
        (content.into(), extra)
    }

    /// A camera to connect: its transport, model and where it is. While it
    /// connects a spinner takes the glyph; if it failed, the error takes
    /// the detail line.
    fn welcome_card<'a>(
        &self,
        kind: Icon,
        title: &'a str,
        detail: String,
        target: &'a str,
        spin: usize,
        p: &'static Palette,
    ) -> Element<'a, Message> {
        let connecting = self.connecting(target);
        let failure = (!connecting)
            .then(|| self.side.connect_failures.get(target))
            .flatten();
        let (glyph, detail, ink): (Element<'a, Message>, String, Color) = if connecting {
            (
                icon::spinner(16.0, p.secondary, spin),
                "Connecting…".into(),
                p.secondary,
            )
        } else if let Some((error, _)) = failure {
            (
                Level::Error.mark(20.0, p),
                first_line(error).to_owned(),
                Level::Error.color(p),
            )
        } else {
            (icon(kind, 20.0, p.accent), detail, p.secondary)
        };
        let card = button(
            row![
                container(glyph).center_x(20).center_y(20),
                column![
                    one_line(title, style::BODY, style::MEDIUM, p.text).width(Fill),
                    one_line(detail, style::CAPTION, style::SANS, ink).width(Fill),
                ]
                .spacing(2)
                .width(Fill),
                icon(Icon::ChevronRight, 13.0, p.tertiary),
            ]
            .spacing(12)
            .align_y(Alignment::Center),
        )
        .width(Fill)
        .padding([10, 14])
        .style(style::card)
        .on_press_maybe((!connecting).then(|| Message::CameraRow(target.to_owned())));
        match failure {
            Some((error, _)) => tip(card, format!("Could not connect: {error}")),
            None => card.into(),
        }
    }
}

/// `content` of `height` growing in as `t` goes from 0 to 1, pushing what
/// follows down; as it is at 1, and under Reduce Motion, which only fades.
fn grow<'a>(content: Element<'a, Message>, height: f32, t: f32) -> Element<'a, Message> {
    if t >= 1.0 || motion::reduce_motion() {
        return content;
    }
    container(content).max_height(height * t).clip(true).into()
}

/// The card slot while discovery runs: where found cameras will appear.
fn searching<'a>(spin: usize, p: &'static Palette) -> Element<'a, Message> {
    container(
        row![
            container(icon::spinner(16.0, p.secondary, spin))
                .center_x(20)
                .center_y(20),
            column![
                text("Searching…")
                    .size(style::BODY)
                    .font(style::MEDIUM)
                    .color(p.secondary),
                text("This computer, the network and USB")
                    .size(style::CAPTION)
                    .color(p.secondary),
            ]
            .spacing(2)
            .width(Fill),
        ]
        .spacing(12)
        .align_y(Alignment::Center),
    )
    .width(Fill)
    .height(CARD)
    .padding([10, 14])
    .style(move |_| container::Style {
        border: iced::Border {
            color: p.hairline,
            width: 1.0,
            radius: style::RADIUS_MEDIUM.into(),
        },
        ..container::Style::default()
    })
    .into()
}

/// The card slot once discovery found nothing: what to check.
fn not_found<'a>(p: &'static Palette) -> Element<'a, Message> {
    container(
        column![
            text("Not seeing your camera?")
                .size(style::SMALL)
                .font(style::MEDIUM)
                .color(p.secondary),
            text(
                "GigE cameras need an address on this computer's subnet. \
                 USB3 cameras need system access."
            )
            .size(style::CAPTION)
            .color(p.secondary)
            .align_x(Alignment::Center),
        ]
        .spacing(2)
        .align_x(Alignment::Center),
    )
    .width(Fill)
    .height(CARD)
    .center_y(CARD)
    .into()
}
