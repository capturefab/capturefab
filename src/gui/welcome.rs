//! What the stage shows before pictures: the welcome screen with no camera
//! connected, and a connected camera before its first frame.
use super::*;
use iced::widget::{column, hover};
use sidebar::{DISCOVERING, DISCOVERS, ISSUES, place, recent_place};

/// Height of a welcome card: its padding and its title and detail lines.
const CARD: f32 =
    10.0 + style::line_height(style::BODY) + 2.0 + style::line_height(style::CAPTION) + 10.0;
/// Height of the discovery callout: its padding and one line.
const CALLOUT: f32 = 4.0 + style::line_height(style::SMALL) + 4.0;
/// Height of the welcome screen's content with one card and its footer,
/// which sits centered; more grows downward from there, so nothing above
/// the cards ever moves as discovery changes them.
const ANCHORED: f32 = 400.0;
/// How far the welcome screen rises as it fades in.
const RISE: f32 = 24.0;
/// How many cameras the welcome screen lists before "Show all".
const SLOTS: usize = 4;
/// Height of the "Show all" row under the cards: its padding and one line.
const MORE: f32 = 4.0 + style::line_height(style::SMALL) + 4.0;
/// Room under a streaming camera's title for what to check when its first
/// frame is late, kept from the start so the title never moves: a hint of
/// up to three lines and a row of buttons.
const GUIDANCE: f32 = 10.0 + 3.0 * style::line_height(style::SMALL) + 12.0 + 32.0;

/// What the welcome screen offers; see `welcome_choices`.
pub(super) struct Choices<'a> {
    /// Cameras to list as cards: connected ones, then found ones by rank.
    pub(super) found: Vec<&'a CameraInfo>,
    /// Recent cameras not found again, under their own heading.
    pub(super) recent: Vec<&'a Recent>,
    /// Whether the slot says discovery is searching.
    pub(super) searching: bool,
    /// Cameras found but not listed, for want of room.
    pub(super) hidden: usize,
}

/// Where a found camera ranks on the welcome screen: machine-vision
/// cameras first, then network streams, webcams, hub cameras, the
/// simulator, screens and scanners. `camera::discover` keeps its own order,
/// which the CLI reports.
fn welcome_rank(device: &CameraInfo) -> u8 {
    use crate::types::Transport;
    match device.transport {
        Transport::GigE | Transport::Usb3 => 0,
        Transport::Media if crate::hubs::is_source(&device.id) => 3,
        Transport::Media => match host_device(device) {
            None => 1,
            Some(HostKind::Webcam) => 2,
            Some(HostKind::Screen) => 5,
        },
        Transport::Simulator => 4,
        Transport::Scanner => 6,
    }
}

/// A found camera's glyph: what kind of input it is.
fn card_icon(device: &CameraInfo) -> Icon {
    match host_device(device) {
        Some(HostKind::Webcam) => Icon::Webcam,
        Some(HostKind::Screen) => Icon::Monitor,
        None => Icon::transport(device.transport),
    }
}

/// What a streaming camera's user might check when its first frame is late,
/// by transport; suggestions only, as a slow camera may simply be slow.
fn first_frame_hint(transport: crate::types::Transport) -> Option<&'static str> {
    use crate::types::Transport;
    match transport {
        Transport::GigE => Some(
            "Frames may be blocked. Check that the firewall allows Capturefab, that the camera is on this computer's subnet, or try a smaller packet size.",
        ),
        Transport::Usb3 => Some(
            "Check the cable and USB3 port, and that the system allows access to the camera.",
        ),
        Transport::Media => Some(
            "Check the address and credentials, and for webcams the system's camera permission.",
        ),
        Transport::Simulator | Transport::Scanner => None,
    }
}

impl Workbench {
    /// A connected camera before its first frame. On the first connect the
    /// surface dims from the welcome screen's to the stage's.
    pub(super) fn ready(&self, p: &'static Palette) -> Element<'_, Message> {
        let streaming = self.snapshot.streaming;
        let scanner = self.scanner_selected();
        // A stream whose first frame is past due, by the same measure as
        // the stall badge: never for triggered cameras or a busy worker.
        let id = self.snapshot.active_camera.as_deref();
        let late = streaming
            && !scanner
            && id.is_some_and(|id| self.stalled(id).is_some() && self.reconnecting(id).is_none());
        let mut ready = column![
            icon(
                if scanner { Icon::Scan } else { Icon::Camera },
                40.0,
                style::ON_STAGE_SECONDARY
            ),
            space().height(6),
            text(if scanner && self.capturing_here() {
                "Scanning document…"
            } else if scanner {
                "Ready to scan a document"
            } else if late {
                "No picture yet"
            } else if streaming {
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
        if !streaming || scanner {
            let mut actions = row![].spacing(10).align_y(Alignment::Center);
            if !scanner {
                actions = actions.push(
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
                );
            }
            ready = ready.push(space().height(8)).push(
                actions.push(
                    button(
                        text(if scanner {
                            "Scan & save"
                        } else {
                            "Capture one frame"
                        })
                        .size(style::BODY),
                    )
                    .padding([7, 12])
                    .style(style::on_glass(false, 1.0))
                    .on_press_maybe(
                        (!self.capturing_here()
                            && self.validate_input(&self.capture_command(false)).is_ok())
                        .then_some(Message::Capture),
                    ),
                ),
            );
        }
        if streaming && !scanner {
            ready = ready.push(
                container(self.first_frame_guidance(late))
                    .height(GUIDANCE)
                    .center_x(Fill)
                    .align_top(GUIDANCE),
            );
        }
        let t = self.side.stage_in.get(self.now);
        let surface = style::mix(p.base, p.stage, t);
        // Floating panels cover the stage's center in compact windows.
        // Keep its guidance and actions in the room the user can see.
        let (left, right) = if self.panels_docked() {
            (0.0, 0.0)
        } else {
            (
                self.sidebar_slide.lerp(0.0, INSPECTOR, self.now),
                self.inspector_slide.lerp(0.0, INSPECTOR, self.now),
            )
        };
        let ready = center(ready).padding(iced::Padding {
            top: motion::rise(16.0, t),
            left,
            right,
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

    /// Under a streaming camera's title, once its first frame is `late`:
    /// what to check for its transport, and what to do about it.
    fn first_frame_guidance(&self, late: bool) -> Element<'_, Message> {
        let Some(id) = self.snapshot.active_camera.clone().filter(|_| late) else {
            return space().height(0).into();
        };
        let transport = self
            .snapshot
            .connected
            .as_ref()
            .map(|camera| camera.transport);
        let mut guidance = column![].spacing(12).align_x(Alignment::Center);
        if let Some(hint) = transport.and_then(first_frame_hint) {
            guidance = guidance.push(
                text(hint)
                    .size(style::SMALL)
                    .color(style::ON_STAGE_SECONDARY)
                    .align_x(Alignment::Center)
                    .width(Fill),
            );
        }
        let action = |label: &'static str, on: Option<Message>| {
            button(text(label).size(style::BODY))
                .name(label)
                .padding([7, 12])
                .style(style::on_glass(false, 1.0))
                .on_press_maybe(on)
        };
        let mut actions = row![].spacing(10).align_y(Alignment::Center);
        if transport == Some(crate::types::Transport::GigE) {
            actions = actions.push(action(
                "Check network",
                (!self.pending(Job::NetworkScan)).then_some(Message::ScanNetwork),
            ));
        }
        actions = actions
            .push(action("Activity", Some(Message::CameraActivity(id))))
            .push(action("Stop stream", Some(Message::ToggleStream)));
        guidance.push(actions).max_width(440).into()
    }

    pub(super) fn welcome(&self, p: &'static Palette) -> Element<'_, Message> {
        let t = self.welcome.get(self.now);
        let block = responsive(move |size| {
            let compact = size.height < 600.0;
            let (content, extra) = self.welcome_content_for(p, compact);
            // Centered with one card; taller content grows downward until
            // it would run off the bottom, then rises just enough.
            let room = size.height - 2.0 * RISE;
            let top = ((size.height - ANCHORED) / 2.0)
                .min(room - ANCHORED - extra)
                .max(RISE);
            scrollable(container(content).center_x(Fill).padding(iced::Padding {
                top: top + motion::rise(RISE, t),
                right: RISE,
                bottom: 24.0,
                left: RISE,
            }))
            .id("welcome")
            .height(Fill)
            .style(style::scroll)
            .into()
        });
        veil(container(block).clip(true), p.base, 0.0, t)
    }

    /// The welcome screen's column, and roughly how much taller than
    /// `ANCHORED` it is.
    #[cfg(test)]
    pub(super) fn welcome_content(&self, p: &'static Palette) -> (Element<'_, Message>, f32) {
        self.welcome_content_for(p, false)
    }

    /// What the welcome screen offers: the cameras connected, found and
    /// recent ones it lists as cards, whether it says it is searching, and
    /// how many found it has no room for.
    pub(super) fn welcome_choices(&self) -> Choices<'_> {
        let connected = self.snapshot.cameras.iter().map(|camera| &camera.info);
        let mut discovered: Vec<&CameraInfo> = self
            .listed_devices()
            .filter(|device| !connected.clone().any(|camera| camera.id == device.id))
            .collect();
        // Stable, so cameras of one rank keep discovery's order.
        discovered.sort_by_key(|device| welcome_rank(device));
        let total = connected.len() + discovered.len();
        let shown = if self.side.welcome_all {
            total
        } else {
            total.min(SLOTS)
        };
        let found: Vec<&CameraInfo> = connected.clone().chain(discovered).take(shown).collect();
        let recent: Vec<&Recent> = self
            .recent
            .iter()
            .filter(|recent| {
                !connected
                    .clone()
                    .chain(self.listed_devices())
                    .any(|device| {
                        device.id == recent.target
                            || device.serial == recent.target
                            || device.address.as_deref() == Some(recent.target.as_str())
                    })
            })
            // With nothing found, the searching or not-found slot holds the
            // first card's place, so there are never more than four slots.
            .take(SLOTS.saturating_sub(found.len().max(1)))
            .collect();
        // A quick search keeps what the screen said; its button spins.
        let searching = self.discovering_aloud() && self.searching_shown();
        Choices {
            hidden: total - found.len(),
            found,
            recent,
            searching,
        }
    }

    /// How the welcome screen offers a simulated camera; see
    /// `simulator_offer`.
    #[cfg(test)]
    fn welcome_simulator(&self) -> Option<bool> {
        let choices = self.welcome_choices();
        welcome_simulator_for(&choices.found, &choices.recent, choices.searching)
    }

    fn welcome_content_for(
        &self,
        p: &'static Palette,
        compact: bool,
    ) -> (Element<'_, Message>, f32) {
        let now = self.now;
        let discovering = self.discovering_aloud();
        let spin = self.spin();
        let Choices {
            found,
            recent,
            searching: searching_shown,
            hidden,
        } = self.welcome_choices();
        // The next step, not the state: the cards, the simulator and the
        // address field below carry the actions.
        let subtitle = if !found.is_empty() {
            "Choose a camera to see a live image.".to_owned()
        } else if searching_shown {
            DISCOVERING.into()
        } else {
            "No cameras found yet.".into()
        };
        let mut extra = 0.0;
        // A short window keeps its room for the cameras and the footer.
        let mark: Element<'_, Message> = if compact {
            space().height(0).into()
        } else {
            column![icon(Icon::Mark, 44.0, p.accent), space().height(10)]
                .align_x(Alignment::Center)
                .into()
        };
        let mut content = column![
            mark,
            text("Your capture workbench")
                .size(style::DISPLAY)
                .font(style::BOLD)
                .color(p.text),
            space().height(if compact { 2 } else { 6 }),
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
        content = content
            .push(issues)
            .push(space().height(if compact { 6 } else { 10 }));
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
            let connected = self.snapshot.cameras.iter().any(|c| c.info.id == device.id);
            let card = self.welcome_card(
                card_icon(device),
                &device.model,
                if connected {
                    format!("Connected · {}", place(device))
                } else {
                    place(device)
                },
                &device.id,
                self.connecting_to(device),
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
        if hidden > 0 {
            let total = found.len() + hidden;
            cards = cards.push(
                container(
                    button(text(format!("Show all {total} cameras")).size(style::SMALL))
                        .name(format!("Show all {total} cameras"))
                        .style(style::link)
                        .padding([4, 6])
                        .on_press(Message::ShowAllCameras),
                )
                .center_x(Fill),
            );
            extra += MORE + 8.0;
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
            let forget = tip(
                button(icon(Icon::Close, 11.0, p.secondary))
                    .name(format!("Forget {}", entry.label))
                    .padding(5)
                    .style(style::plain)
                    .on_press(Message::Forget(entry.target.clone())),
                "Forget",
            );
            cards = cards.push(hover(
                self.welcome_card(
                    Icon::transport(entry.transport),
                    &entry.label,
                    recent_place(entry),
                    &entry.target,
                    self.connecting(&entry.target),
                    p,
                ),
                container(forget)
                    .align_right(Fill)
                    .center_y(Fill)
                    .padding(iced::Padding {
                        right: 36.0,
                        ..iced::Padding::ZERO
                    }),
            ));
        }
        // `ANCHORED` holds the first slot: a found camera's card, or the
        // searching or not-found slot that stands in for one.
        let slots = found.len().max(1) + recent.len();
        extra += (slots - 1) as f32 * (CARD + 8.0);
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
        let simulator = welcome_simulator_for(&found, &recent, searching_shown);
        // Search sits beside the address field. The simulator keeps its own
        // slot, so it can come and go (once a simulated camera is listed)
        // without resetting the field; it is the primary action only on an
        // empty screen, where it is the quickest way in.
        let search = tip(
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
        );
        let gap = if compact { 8.0 } else { 14.0 };
        let simulator = simulator.map(|primary| {
            container(action(
                Icon::Cube,
                "Try a simulated camera",
                Message::TrySimulator,
                self.connecting(&self.free_simulator()),
                primary,
            ))
            .padding(iced::Padding {
                bottom: gap,
                ..iced::Padding::ZERO
            })
        });
        let content = content
            .push(cards)
            .push(space().height(gap))
            .push(simulator)
            .push(self.picker_footer(p, search));
        (content.into(), extra)
    }

    /// A camera to connect: its transport, model and where it is. While it
    /// is `connecting` a spinner takes the glyph; if it failed, the error
    /// takes the detail line.
    fn welcome_card<'a>(
        &self,
        kind: Icon,
        title: &'a str,
        detail: String,
        target: &'a str,
        connecting: bool,
        p: &'static Palette,
    ) -> Element<'a, Message> {
        let failure = (!connecting)
            .then(|| self.side.connect_failures.get(target))
            .flatten();
        let (glyph, detail, ink): (Element<'a, Message>, String, Color) = if connecting {
            (
                icon::spinner(16.0, p.secondary, self.spin()),
                "Connecting…".into(),
                p.secondary,
            )
        } else if let Some((error, _)) = failure {
            (
                Level::Error.mark(20.0, p),
                capitalize(first_line(error)),
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

/// `simulator_offer` for what the welcome screen lists.
fn welcome_simulator_for(
    found: &[&CameraInfo],
    recent: &[&Recent],
    searching: bool,
) -> Option<bool> {
    // A simulated camera already offered as a card needs no button.
    let listed = sim_listed(found.iter().copied(), recent.iter().copied());
    simulator_offer(found.is_empty() && recent.is_empty(), listed, searching)
}

/// Whether a simulated camera is already offered among `devices` found or
/// `recent` ones, so nothing needs to suggest trying the simulator.
pub(super) fn sim_listed<'a>(
    mut devices: impl Iterator<Item = &'a CameraInfo>,
    mut recent: impl Iterator<Item = &'a Recent>,
) -> bool {
    devices.any(|device| {
        device.transport == crate::types::Transport::Simulator || device.id.starts_with("sim:")
    }) || recent.any(|recent| recent.target.starts_with("sim:"))
}

/// How the welcome screen offers a simulated camera: `None` when one is
/// already listed, `Some(true)` as the primary action when there is nothing
/// to choose (`empty`) and nothing is being searched for, else `Some(false)`.
fn simulator_offer(empty: bool, sim_listed: bool, searching: bool) -> Option<bool> {
    (!sim_listed).then_some(empty && !searching)
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

/// The card slot once discovery found nothing: what to check, and where
/// the quick guide says more.
fn not_found<'a>(p: &'static Palette) -> Element<'a, Message> {
    container(
        column![
            row![
                text("Not seeing your camera?")
                    .size(style::SMALL)
                    .font(style::MEDIUM)
                    .color(p.secondary),
                button(text("Connection tips").size(style::SMALL))
                    .style(style::link)
                    .padding([0, 4])
                    .on_press(Message::HelpMore(true)),
            ]
            .spacing(2)
            .align_y(Alignment::Center),
            text("GigE needs this computer's subnet; USB3 needs system access.")
                .size(style::CAPTION)
                .color(p.secondary)
                .align_x(Alignment::Center),
        ]
        .spacing(2)
        .align_x(Alignment::Center),
    )
    .width(Fill)
    .height(CARD)
    .center_x(Fill)
    .center_y(CARD)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Transport;

    /// The scene lists simulated cameras as cards, so the simulator is not
    /// offered again; the quieter links stay in view.
    #[test]
    fn compact_welcome_keeps_onboarding_actions_visible_and_lower_content_scrollable() {
        use crate::gui::destinations::layout_tests::{
            apply, prepare_artifact_frame, render_artifact_scaled, renderer,
        };
        use iced::advanced::{Layout, layout, widget::Tree, widget::operation};
        let mut renderer = renderer();
        for scale in prefs::UiScale::ALL {
            let mut app = bench();
            app.ui_scale = scale;
            app.apply_scene(&["welcome", "side-recent"]);
            app.scene.patch(&mut app.snapshot);
            app.tick_side();
            app.now += Duration::from_secs(3);
            app.welcome.set(1.0);
            let size = Size::new(
                900.0 / scale.factor(),
                620.0 / scale.factor() - BAR - TOOLBAR,
            );
            let mut view = app.welcome(Palette::of(false));
            let mut tree = Tree::new(view.as_widget());
            let node = view.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, size),
            );
            prepare_artifact_frame(&mut view, &mut tree, &node, &renderer, size);
            let (snapshot, _, _) = accessibility::snapshot_widget(
                view.as_widget_mut(),
                &mut tree,
                Layout::new(&node),
                &renderer,
            );
            assert!(
                !snapshot
                    .nodes
                    .iter()
                    .any(|(_, control)| control.label() == Some("Try a simulated camera")),
                "simulated cameras are listed, so the simulator is not offered at {scale}"
            );
            for label in [
                Action::Discover.hint(DISCOVERS, Os::CURRENT),
                "Ask the assistant".into(),
                "Connect an agent".into(),
            ] {
                let action = snapshot
                    .nodes
                    .iter()
                    .find(|(_, control)| {
                        control.role() == accessibility::Role::Button
                            && control.label() == Some(label.as_str())
                    })
                    .unwrap_or_else(|| panic!("{label} is visible in compact welcome at {scale}"));
                // Larger interface sizes scroll to them; see below.
                if scale != prefs::UiScale::Standard {
                    continue;
                }
                let bounds = action.1.bounds().unwrap();
                assert!(
                    bounds.height() >= 18.0
                        && bounds.x0 >= 0.0
                        && bounds.y0 >= 0.0
                        && bounds.x1 <= f64::from(size.width) + 0.5
                        && bounds.y1 <= f64::from(size.height) + 0.5,
                    "{label} fits at {scale}: {bounds:?}, viewport {size:?}"
                );
            }
            render_artifact_scaled(
                &format!("welcome-onboarding-{scale}"),
                &view,
                &tree,
                &node,
                &mut renderer,
                size,
                scale.factor(),
            );
            drop(view);

            // Every source remains in the welcome body. A genuinely short
            // viewport can still reveal its footer.
            let short = Size::new(400.0, 220.0);
            let mut view = app.welcome(Palette::of(false));
            let mut tree = Tree::new(view.as_widget());
            let node = view.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, short),
            );
            prepare_artifact_frame(&mut view, &mut tree, &node, &renderer, short);
            apply(
                &mut view,
                &mut tree,
                &node,
                &renderer,
                operation::scrollable::snap_to(
                    "welcome".into(),
                    operation::scrollable::RelativeOffset {
                        x: None,
                        y: Some(1.0),
                    },
                ),
            );
            let (after, _, _) = accessibility::snapshot_widget(
                view.as_widget_mut(),
                &mut tree,
                Layout::new(&node),
                &renderer,
            );
            let settings = after
                .nodes
                .iter()
                .find(|(_, control)| control.label() == Some("Settings"))
                .expect("real scrolling reaches the footer");
            let bounds = settings.1.bounds().unwrap();
            assert!(bounds.y0 >= 0.0 && bounds.y1 <= f64::from(short.height) + 0.5);
        }
    }

    fn bench() -> Workbench {
        Workbench::new(SessionHandle::new(), "test".into(), true, None)
    }

    #[test]
    fn ready_guidance_and_actions_fit_beside_either_floating_panel_at_every_scale() {
        use crate::gui::destinations::layout_tests::{prepare_artifact_frame, renderer};
        use iced::advanced::{Layout, layout, widget::Tree};
        let renderer = renderer();
        for (scanner, guidance, action) in [
            (true, "Ready to scan a document", "Scan & save"),
            (false, "Ready for your first frame", "Capture one frame"),
        ] {
            for scale in prefs::UiScale::ALL {
                for sidebar in [true, false] {
                    let mut app = bench();
                    let mut camera = crate::transport::simulator::info();
                    if scanner {
                        camera.transport = Transport::Scanner;
                        camera.profile.kind = CameraKind::DocumentScanner;
                    }
                    app.snapshot.active_camera = Some(camera.id.clone());
                    app.snapshot.connected = Some(camera);
                    app.ui_scale = scale;
                    app.width = 900.0 / scale.factor();
                    app.sidebar_peek = sidebar;
                    app.inspector_peek = !sidebar;
                    app.sidebar_slide.set(if sidebar { 1.0 } else { 0.0 });
                    app.inspector_slide.set(if sidebar { 0.0 } else { 1.0 });
                    let size = Size::new(app.width, 620.0 / scale.factor() - BAR - TOOLBAR);
                    let mut view = app.ready(Palette::of(false));
                    let mut tree = Tree::new(view.as_widget());
                    let node = view.as_widget_mut().layout(
                        &mut tree,
                        &renderer,
                        &layout::Limits::new(Size::ZERO, size),
                    );
                    prepare_artifact_frame(&mut view, &mut tree, &node, &renderer, size);
                    let (snapshot, _, _) = accessibility::snapshot_widget(
                        view.as_widget_mut(),
                        &mut tree,
                        Layout::new(&node),
                        &renderer,
                    );
                    let (left, right) = if sidebar {
                        (INSPECTOR, size.width)
                    } else {
                        (0.0, size.width - INSPECTOR)
                    };
                    let mut labels = vec![guidance, action];
                    if !scanner {
                        labels.push("Start stream");
                    }
                    for label in labels {
                        let bounds = snapshot
                            .nodes
                            .iter()
                            .find(|(_, control)| control.label() == Some(label))
                            .and_then(|(_, control)| control.bounds())
                            .unwrap_or_else(|| panic!("{label} is visible"));
                        assert!(
                            bounds.x0 >= f64::from(left) - 0.5
                                && bounds.x1 <= f64::from(right) + 0.5
                                && bounds.y0 >= 0.0
                                && bounds.y1 <= f64::from(size.height) + 0.5,
                            "{label} fits beside {} at {scale}: {bounds:?}",
                            if sidebar { "Capture" } else { "Features" }
                        );
                    }
                }
            }
        }
    }

    /// What to check when the first frame is late fits beside either
    /// floating panel, at every interface size, in a compact window.
    #[test]
    fn late_first_frame_guidance_fits_beside_either_floating_panel_at_every_scale() {
        for scale in prefs::UiScale::ALL {
            for sidebar in [true, false] {
                let mut app = bench();
                app.pending.clear();
                let id = streaming(&mut app, Transport::GigE);
                app.now += Duration::from_secs(6);
                assert!(app.stalled(&id).is_some());
                app.ui_scale = scale;
                app.width = 900.0 / scale.factor();
                app.sidebar_peek = sidebar;
                app.inspector_peek = !sidebar;
                app.sidebar_slide.set(if sidebar { 1.0 } else { 0.0 });
                app.inspector_slide.set(if sidebar { 0.0 } else { 1.0 });
                let size = Size::new(app.width, 620.0 / scale.factor() - BAR - TOOLBAR);
                let shown = controls(app.ready(Palette::of(false)), size);
                let (left, right) = if sidebar {
                    (INSPECTOR, size.width)
                } else {
                    (0.0, size.width - INSPECTOR)
                };
                for label in [
                    "No picture yet",
                    "Check network",
                    "Activity",
                    "Stop stream",
                    first_frame_hint(Transport::GigE).unwrap(),
                ] {
                    let bounds = shown
                        .iter()
                        .find(|(name, _, _)| name == label)
                        .map(|(_, _, bounds)| *bounds)
                        .unwrap_or_else(|| panic!("{label} is visible at {scale}"));
                    assert!(
                        bounds.x0 >= f64::from(left) - 0.5
                            && bounds.x1 <= f64::from(right) + 0.5
                            && bounds.y0 >= 0.0
                            && bounds.y1 <= f64::from(size.height) + 0.5,
                        "{label} fits beside {} at {scale}: {bounds:?}",
                        if sidebar { "Capture" } else { "Features" }
                    );
                }
            }
        }
    }

    #[test]
    fn a_new_window_shows_the_picker_with_its_address_field() {
        use crate::gui::destinations::layout_tests::{prepare_artifact_frame, renderer};
        use iced::advanced::{Layout, layout, widget::Tree};
        let renderer = renderer();
        let mut app = bench();
        let camera = liveness::streaming_camera(0, 0, 0.0);
        app.snapshot.cameras = vec![camera.clone()];
        app.project_local_camera();
        assert!(app.local_camera.is_none() && app.snapshot.connected.is_none());
        // The camera it showed is still connected, and offered as a card.
        let found = app.welcome_choices().found;
        assert_eq!(found[0].id, camera.info.id);
        app.welcome.set(1.0);
        let size = Size::new(900.0, 620.0 - BAR - TOOLBAR);
        let mut view = app.welcome(Palette::of(false));
        let mut tree = Tree::new(view.as_widget());
        let node = view.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, size),
        );
        prepare_artifact_frame(&mut view, &mut tree, &node, &renderer, size);
        let (snapshot, _, _) = accessibility::snapshot_widget(
            view.as_widget_mut(),
            &mut tree,
            Layout::new(&node),
            &renderer,
        );
        for removed in [
            "Open data sources",
            "Live signals",
            "Protected source profiles",
        ] {
            assert!(
                snapshot
                    .nodes
                    .iter()
                    .all(|(_, node)| node.label() != Some(removed)),
                "The camera picker must not expose {removed}."
            );
        }
        for label in ["Camera address", "Settings"] {
            let bounds = snapshot
                .nodes
                .iter()
                .find(|(_, node)| node.label() == Some(label))
                .and_then(|(_, node)| node.bounds())
                .unwrap_or_else(|| panic!("{label}"));
            assert!(
                bounds.y1 <= f64::from(size.height) + 0.5,
                "{label} fits without scrolling: {bounds:?}"
            );
        }
    }

    #[test]
    fn the_simulator_is_offered_once_and_leads_only_on_an_empty_screen() {
        assert_eq!(simulator_offer(true, false, false), Some(true));
        assert_eq!(simulator_offer(true, false, true), Some(false), "searching");
        assert_eq!(
            simulator_offer(false, false, false),
            Some(false),
            "a card leads"
        );
        assert_eq!(simulator_offer(false, true, false), None, "already listed");
    }

    /// With nothing found, nothing recent and no search, the simulator is
    /// the primary action, and the not-found slot links to the guide's
    /// connection notes, all in the viewport.
    #[test]
    fn a_first_run_leads_with_the_simulator_and_links_connection_tips() {
        use crate::gui::destinations::layout_tests::{prepare_artifact_frame, renderer};
        use iced::advanced::{Layout, layout, widget::Tree};
        use iced::futures::StreamExt;
        let renderer = renderer();
        let mut app = bench();
        app.pending.clear();
        app.welcome.set(1.0);
        assert!(app.snapshot.devices.is_empty() && app.recent.is_empty());
        assert!(!app.pending(Job::Discover), "not searching");
        let size = Size::new(900.0, 620.0 - BAR - TOOLBAR);
        let mut view = app.welcome(Palette::of(false));
        let mut tree = Tree::new(view.as_widget());
        let node = view.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, size),
        );
        prepare_artifact_frame(&mut view, &mut tree, &node, &renderer, size);
        let (snapshot, bridge, window) = accessibility::snapshot_widget(
            view.as_widget_mut(),
            &mut tree,
            Layout::new(&node),
            &renderer,
        );
        let visible = |label: &str| {
            let (id, control) = snapshot
                .nodes
                .iter()
                .find(|(_, control)| {
                    control.role() == accessibility::Role::Button && control.label() == Some(label)
                })
                .unwrap_or_else(|| panic!("{label} is on the first-run welcome"));
            let bounds = control.bounds().unwrap();
            assert!(
                bounds.x0 >= 0.0
                    && bounds.y0 >= 0.0
                    && bounds.x1 <= f64::from(size.width) + 0.5
                    && bounds.y1 <= f64::from(size.height) + 0.5,
                "{label} is in the viewport: {bounds:?}"
            );
            *id
        };
        // Primary from the very choices the view lists.
        assert_eq!(app.welcome_simulator(), Some(true), "primary");
        visible(&Action::Discover.hint(DISCOVERS, Os::CURRENT));
        visible("Try a simulated camera");
        visible("Settings");
        let tips = visible("Connection tips");
        let task = bridge.dispatch(accessibility::Request {
            window,
            replay: None,
            action: accesskit::ActionRequest {
                action: accesskit::Action::Click,
                target: tips,
                data: None,
            },
        });
        let mut actions = iced_runtime::task::into_stream(task).unwrap();
        assert!(matches!(
            iced::futures::executor::block_on(actions.next()),
            Some(iced_runtime::Action::Output(Message::HelpMore(true)))
        ));
    }

    fn recent(target: &str) -> Recent {
        Recent {
            target: target.into(),
            label: "acA1920-40gc".into(),
            detail: format!("GigE · {target}"),
            transport: Transport::GigE,
        }
    }

    #[test]
    fn connected_and_discovered_cameras_do_not_repeat_as_recent_aliases() {
        let mut app = bench();
        let mut camera = liveness::streaming_camera(0, 0, 0.0);
        camera.info.id = "camera-id".into();
        camera.info.serial = "CAMERA-SERIAL".into();
        camera.info.address = Some("192.168.1.20".into());
        app.recent = ["camera-id", "CAMERA-SERIAL", "192.168.1.20", "192.168.1.21"]
            .into_iter()
            .map(recent)
            .collect();

        // Another window can keep a camera connected after discovery has
        // stopped advertising it. Its existing card must still replace
        // every recent alias, so those aliases cannot reconnect it.
        app.snapshot.cameras = vec![camera.clone()];
        let Choices { found, recent, .. } = app.welcome_choices();
        assert_eq!(found.len(), 1);
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].target, "192.168.1.21");

        app.snapshot.cameras.clear();
        app.snapshot.devices = vec![camera.info];
        let Choices { found, recent, .. } = app.welcome_choices();
        assert_eq!(found.len(), 1);
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].target, "192.168.1.21");
    }

    /// The labelled controls `view` lays out in `size`, with their bounds.
    fn controls(
        mut view: Element<'_, Message>,
        size: Size,
    ) -> Vec<(String, accessibility::Role, accesskit::Rect)> {
        use crate::gui::destinations::layout_tests::{prepare_artifact_frame, renderer};
        use iced::advanced::{Layout, layout, widget::Tree};
        let renderer = renderer();
        let mut tree = Tree::new(view.as_widget());
        let node = view.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, size),
        );
        prepare_artifact_frame(&mut view, &mut tree, &node, &renderer, size);
        let (snapshot, _, _) = accessibility::snapshot_widget(
            view.as_widget_mut(),
            &mut tree,
            Layout::new(&node),
            &renderer,
        );
        snapshot
            .nodes
            .iter()
            .filter_map(|(_, control)| {
                Some((
                    control.label()?.to_owned(),
                    control.role(),
                    control.bounds()?,
                ))
            })
            .collect()
    }

    fn mixed() -> Vec<CameraInfo> {
        let device = |id: &str, transport: Transport, model: &str| CameraInfo {
            id: id.into(),
            transport,
            vendor: "macos".into(),
            model: model.into(),
            serial: id.into(),
            address: Some(id.into()),
            profile: Default::default(),
        };
        // In discovery's ID order, which puts this computer's inputs first.
        vec![
            device(
                "avfoundation:Capture screen 0",
                Transport::Media,
                "Capture screen 0",
            ),
            device("avfoundation:FaceTime", Transport::Media, "FaceTime"),
            device("avfoundation:C920", Transport::Media, "C920"),
            device("gige:10.0.0.5", Transport::GigE, "Area camera"),
            device("scanner:flatbed", Transport::Scanner, "Flatbed"),
            device("usb:XYZ", Transport::Usb3, "USB3 camera"),
        ]
    }

    /// Machine-vision cameras lead, screens and scanners come last, and the
    /// welcome screen says how many more it found than it lists.
    #[test]
    fn the_welcome_ranks_cameras_by_kind_and_offers_the_rest() {
        let mut app = bench();
        app.pending.clear();
        app.snapshot.devices = mixed();
        let choices = app.welcome_choices();
        let ids: Vec<&str> = choices.found.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "gige:10.0.0.5",
                "usb:XYZ",
                "avfoundation:FaceTime",
                "avfoundation:C920"
            ]
        );
        assert_eq!(choices.hidden, 2, "the screen and the scanner");
        assert!(choices.recent.is_empty());
        // A connected camera still comes first.
        let mut connected = liveness::streaming_camera(0, 0, 0.0);
        connected.info = app.snapshot.devices[0].clone();
        app.snapshot.cameras = vec![connected];
        let choices = app.welcome_choices();
        assert_eq!(choices.found[0].id, "avfoundation:Capture screen 0");
        assert_eq!(choices.found[1].id, "gige:10.0.0.5");
        assert_eq!(choices.hidden, 2);
        app.snapshot.cameras.clear();

        app.welcome.set(1.0);
        let size = Size::new(900.0, 620.0 - BAR - TOOLBAR);
        let shown = controls(app.welcome(Palette::of(false)), size);
        assert!(
            shown.iter().any(|(label, role, _)| label == "Show all 6 cameras"
                && *role == accessibility::Role::Button),
            "{shown:?}"
        );
        let _ = app.handle_message(Message::ShowAllCameras);
        let choices = app.welcome_choices();
        assert_eq!(choices.found.len(), 6);
        assert_eq!(choices.hidden, 0);
        assert_eq!(
            choices.found.last().map(|d| d.id.as_str()),
            Some("scanner:flatbed")
        );
        assert_eq!(choices.found[4].id, "avfoundation:Capture screen 0");
        let shown = controls(app.welcome(Palette::of(false)), size);
        assert!(!shown.iter().any(|(label, _, _)| label.starts_with("Show all")));
        // Leaving the welcome screen folds the list again.
        app.snapshot.connected = Some(crate::transport::simulator::info());
        app.tick_side();
        assert!(!app.side.welcome_all);

        // Four or fewer need no link.
        let mut app = bench();
        app.snapshot.devices = mixed()[..4].to_vec();
        assert_eq!(app.welcome_choices().hidden, 0);
        app.welcome.set(1.0);
        let shown = controls(app.welcome(Palette::of(false)), size);
        assert!(!shown.iter().any(|(label, _, _)| label.starts_with("Show all")));
    }

    /// The camera path leads: the assistant and agent entry points sit
    /// below the address field, quietly, and still in view.
    #[test]
    fn the_camera_path_leads_and_the_assistant_follows() {
        let mut app = bench();
        app.pending.clear();
        app.welcome.set(1.0);
        let size = Size::new(900.0, 620.0 - BAR - TOOLBAR);
        let shown = controls(app.welcome(Palette::of(false)), size);
        let top = |name: &str| {
            shown
                .iter()
                .find(|(label, _, _)| label == name)
                .map(|(_, _, bounds)| bounds.y0)
                .unwrap_or_else(|| panic!("{name} in {shown:?}"))
        };
        let field = top("Camera address");
        assert!(top("Try a simulated camera") < field);
        assert!(top("Ask the assistant") > field);
        assert!(top("Connect an agent") > field);
        // The simulator is the one primary action, and only here.
        assert_eq!(app.welcome_simulator(), Some(true));
    }

    /// The not-found slot sits centered under the headline, like the cards.
    #[test]
    fn the_not_found_slot_is_centered() {
        let mut app = bench();
        app.pending.clear();
        app.welcome.set(1.0);
        let size = Size::new(900.0, 620.0 - BAR - TOOLBAR);
        let shown = controls(app.welcome(Palette::of(false)), size);
        let bounds = |name: &str| {
            shown
                .iter()
                .find(|(label, _, _)| label == name)
                .map(|(_, _, bounds)| *bounds)
                .unwrap_or_else(|| panic!("{name} in {shown:?}"))
        };
        let tips = bounds("Connection tips");
        let field = bounds("Camera address");
        let search = bounds(&Action::Discover.hint(DISCOVERS, Os::CURRENT));
        let center = (field.x0 + search.x1) / 2.0;
        // "Not seeing your camera?" sits left of the link, so the row's
        // center is left of the link's; but far from the left edge.
        assert!(
            tips.x0 > center - 20.0 && tips.x0 < center + 120.0,
            "tips {tips:?} around {center}"
        );
    }

    fn streaming(app: &mut Workbench, transport: Transport) -> String {
        let mut camera = liveness::streaming_camera(0, 0, 0.0);
        camera.info.transport = transport;
        let id = camera.info.id.clone();
        app.snapshot.streaming = true;
        app.snapshot.active_camera = Some(id.clone());
        app.snapshot.connected = Some(camera.info.clone());
        app.snapshot.cameras = vec![camera];
        app.observe_liveness();
        id
    }

    /// While a stream's first frame is late, the stage says what to check
    /// for its transport, and offers what to do; never before its limit.
    #[test]
    fn a_late_first_frame_says_what_to_check() {
        let size = Size::new(900.0, 620.0 - BAR - TOOLBAR);
        for (transport, network) in [
            (Transport::GigE, true),
            (Transport::Usb3, false),
            (Transport::Media, false),
            (Transport::Simulator, false),
        ] {
            let mut app = bench();
            app.pending.clear();
            let id = streaming(&mut app, transport);
            let labels = |app: &Workbench| -> Vec<String> {
                controls(app.ready(Palette::of(false)), size)
                    .into_iter()
                    .map(|(label, _, _)| label)
                    .collect()
            };
            let before = labels(&app);
            assert!(before.iter().any(|l| l == "Waiting for the first frame…"));
            assert!(!before.iter().any(|l| l == "Stop stream"), "{before:?}");
            app.now += Duration::from_secs(6);
            assert!(app.stalled(&id).is_some(), "{transport}");
            let after = labels(&app);
            assert!(after.iter().any(|l| l == "No picture yet"), "{after:?}");
            assert!(after.iter().any(|l| l == "Stop stream"));
            assert!(after.iter().any(|l| l == "Activity"));
            assert_eq!(
                after.iter().any(|l| l == "Check network"),
                network,
                "{transport}"
            );
            assert_eq!(
                after.iter().any(|l| l.starts_with("Frames may be blocked")),
                network
            );
        }
        // A triggered camera waits as long as it must.
        let mut app = bench();
        app.pending.clear();
        let id = streaming(&mut app, Transport::GigE);
        app.liveness.get_mut(&id).unwrap().pace_triggered_for_tests();
        app.now += Duration::from_secs(60);
        let shown = controls(app.ready(Palette::of(false)), size);
        assert!(!shown.iter().any(|(label, _, _)| label == "No picture yet"));
    }

    #[test]
    fn the_height_estimate_counts_the_slot_that_stands_in_for_a_card() {
        let mut bench = bench();
        let p = &style::LIGHT;
        let extra = |bench: &Workbench| bench.welcome_content(p).1;
        assert_eq!(extra(&bench), 0.0, "the slot alone is anchored");
        bench.recent = vec![recent("10.0.0.1"), recent("10.0.0.2")];
        let waiting = extra(&bench);
        // The slot and two recents under their heading.
        assert_eq!(waiting, 30.0 + 2.0 * (CARD + 8.0));
        // The first camera found takes the slot, so nothing moves.
        bench.snapshot.devices = vec![crate::transport::simulator::info_named("sim:1")];
        assert_eq!(extra(&bench), waiting);
        // Never more than four slots, the stand-in counted.
        bench.snapshot.devices.clear();
        bench.recent = (0..6).map(|n| recent(&format!("10.0.0.{n}"))).collect();
        assert_eq!(extra(&bench), 30.0 + 3.0 * (CARD + 8.0));
    }
}
