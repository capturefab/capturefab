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
const ANCHORED: f32 = 440.0;
/// How far the welcome screen rises as it fades in.
const RISE: f32 = 24.0;

impl Workbench {
    /// A connected camera before its first frame. On the first connect the
    /// surface dims from the welcome screen's to the stage's.
    pub(super) fn ready(&self, p: &'static Palette) -> Element<'_, Message> {
        let streaming = self.snapshot.streaming;
        let scanner = self.scanner_selected();
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
    /// recent ones it lists as cards, and whether it says it is searching.
    fn welcome_choices(&self) -> (Vec<&CameraInfo>, Vec<&Recent>, bool) {
        let connected = self.snapshot.cameras.iter().map(|camera| &camera.info);
        let found: Vec<&CameraInfo> = connected
            .clone()
            .chain(
                self.snapshot
                    .devices
                    .iter()
                    .filter(|device| !connected.clone().any(|camera| camera.id == device.id)),
            )
            .take(4)
            .collect();
        let recent: Vec<&Recent> = self
            .recent
            .iter()
            .filter(|recent| {
                !connected
                    .clone()
                    .chain(self.snapshot.devices.iter())
                    .any(|device| {
                        device.id == recent.target
                            || device.serial == recent.target
                            || device.address.as_deref() == Some(recent.target.as_str())
                    })
            })
            // With nothing found, the searching or not-found slot holds the
            // first card's place, so there are never more than four slots.
            .take(4 - found.len().max(1))
            .collect();
        // A quick search keeps what the screen said; its button spins.
        let searching_shown = self.pending(Job::Discover) && self.searching_shown();
        (found, recent, searching_shown)
    }

    /// How the welcome screen offers a simulated camera; see
    /// `simulator_offer`.
    #[cfg(test)]
    fn welcome_simulator(&self) -> Option<bool> {
        let (found, recent, searching_shown) = self.welcome_choices();
        welcome_simulator_for(&found, &recent, searching_shown)
    }

    fn welcome_content_for(
        &self,
        p: &'static Palette,
        compact: bool,
    ) -> (Element<'_, Message>, f32) {
        let now = self.now;
        let discovering = self.pending(Job::Discover);
        let spin = self.spin();
        let (found, recent, searching_shown) = self.welcome_choices();
        let subtitle = if !found.is_empty() {
            "Choose a camera.".to_owned()
        } else if searching_shown {
            DISCOVERING.into()
        } else {
            "No cameras found. Try the simulator, or enter an address or stream URL.".into()
        };
        let mut extra = 0.0;
        let mut content = column![
            icon(Icon::Mark, if compact { 32.0 } else { 44.0 }, p.accent),
            space().height(if compact { 6 } else { 10 }),
            text("Your capture workbench")
                .size(style::DISPLAY)
                .font(style::BOLD)
                .color(p.text),
            row![
                button(
                    row![
                        icon(Icon::Sparkle, 15.0, Color::WHITE),
                        text("Ask the assistant").size(style::BODY)
                    ]
                    .spacing(7)
                    .align_y(Alignment::Center)
                )
                .name("Ask the assistant")
                .description("Plan or run a capture task using your own provider or local model")
                .style(style::primary)
                .padding([7, 12])
                .on_press(Message::Assistant(assistant::Input::Quick(
                    assistant::quick::Input::Open(true)
                ))),
                button(text("Connect an agent").size(style::BODY))
                    .name("Connect an agent")
                    .description(
                        "Let an external harness control this same workbench through the CLI"
                    )
                    .style(style::secondary)
                    .padding([7, 12])
                    .on_press(Message::AgentSetup(true)),
            ]
            .spacing(8)
            .wrap()
            .vertical_spacing(8),
            space().height(if compact { 4 } else { 12 }),
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
                Icon::transport(device.transport),
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
            let signed_in = self.saved_login_user(&entry.target);
            let forget = tip(
                button(icon(Icon::Close, 11.0, p.secondary))
                    .name(format!("Forget {}", entry.label))
                    .padding(5)
                    .style(style::plain)
                    .on_press(Message::Forget(entry.target.clone())),
                if signed_in.is_some() {
                    "Forget camera and saved sign-in"
                } else {
                    "Forget"
                },
            );
            cards = cards.push(hover(
                self.welcome_card_signed(
                    Icon::transport(entry.transport),
                    &entry.label,
                    recent_place(entry),
                    &entry.target,
                    self.connecting(&entry.target),
                    signed_in,
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
        self.welcome_card_signed(kind, title, detail, target, connecting, None, p)
    }

    /// `welcome_card`, with a lock before the detail line for a camera
    /// whose sign-in is saved as `signed_in`.
    #[allow(clippy::too_many_arguments)]
    fn welcome_card_signed<'a>(
        &self,
        kind: Icon,
        title: &'a str,
        detail: String,
        target: &'a str,
        connecting: bool,
        signed_in: Option<&str>,
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
                    match signed_in.filter(|_| !connecting && failure.is_none()) {
                        Some(user) => Element::from(
                            row![
                                tip(
                                    icon(Icon::Lock, 10.0, p.secondary),
                                    format!("Signed in · user {user}"),
                                ),
                                one_line(detail, style::CAPTION, style::SANS, ink).width(Fill),
                            ]
                            .spacing(4)
                            .align_y(Alignment::Center),
                        ),
                        None => one_line(detail, style::CAPTION, style::SANS, ink)
                            .width(Fill)
                            .into(),
                    },
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
        let (found, _, _) = app.welcome_choices();
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
        let (found, recent, _) = app.welcome_choices();
        assert_eq!(found.len(), 1);
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].target, "192.168.1.21");

        app.snapshot.cameras.clear();
        app.snapshot.devices = vec![camera.info];
        let (found, recent, _) = app.welcome_choices();
        assert_eq!(found.len(), 1);
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].target, "192.168.1.21");
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
