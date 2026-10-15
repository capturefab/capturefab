//! The bottom bar and the activity log above it.
use super::*;
use iced::widget::column;
use std::collections::HashSet;
#[cfg(test)]
mod raw_log_tests;

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
                .name(format!(
                    "Capture destination: {}",
                    self.capture_to.label(&self.output)
                ))
                .padding([4, 6])
                .style(style::plain)
                .on_press(Message::ShowTab(Tab::Capture)),
                Action::CaptureTab.hint("Where captures are saved", os),
            ));
        }
        let unseen = self.chrome.unseen.filter(|_| !self.logs_open);
        let history_button = |undo: bool| {
            let action = if undo { Action::Undo } else { Action::Redo };
            let label = self.history_label(undo);
            let about = label.map_or_else(
                || action.label().to_owned(),
                |label| format!("{} {label}", action.label()),
            );
            tip_above(
                button(text(action.label()).size(style::SMALL))
                    .name(about.clone())
                    .padding([4, 6])
                    .style(style::plain)
                    .on_press_maybe(
                        (label.is_some() && !self.history_busy()).then_some(if undo {
                            Message::Undo
                        } else {
                            Message::Redo
                        }),
                    ),
                action.hint(&about, os),
            )
        };
        let mut activity = stack![tip_above(
            icon_button(
                Icon::Activity,
                16.0,
                if self.logs_open {
                    p.accent_text
                } else {
                    p.secondary
                },
                Message::ToggleActivity,
            ),
            Action::ToggleActivity.hint(
                match unseen {
                    Some(Level::Error) => "Session activity: new errors",
                    Some(_) => "Session activity: new warnings",
                    None if self.client_activity.unread =>
                        "Session activity: new actions from other participants",
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
        } else if self.client_activity.unread && !self.logs_open {
            activity = activity.push(
                container(dot(p.accent, 6.0))
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
                    row![history_button(true), history_button(false)].spacing(2),
                    tip_above(
                        button(text("Analytics").size(style::SMALL))
                            .name("Session analytics for all cameras")
                            .padding([4, 6])
                            .style(style::plain)
                            .on_press(Message::Analytics(true)),
                        "Frame rate, payload throughput and missed frames for this session",
                    ),
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
        let mut room = self.main_width() - 2.0 * (GUTTER - 6.0) - 4.0 * 10.0 - 198.0;
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
        let problem = self.validate_input(&self.capture_command(false)).err();
        let scanner = self.scanner_selected();
        let capturing = self.capturing_here();
        // This user's pending capture is per camera. Other participants can
        // keep capturing while this button queues the user's own request.
        let saved = !capturing && self.just_saved(self.snapshot.active_camera.as_deref());
        let (glyph, label): (Element<'_, Message>, _) = if capturing {
            (
                icon::spinner(14.0, p.secondary, self.spin()),
                if scanner { "Scanning…" } else { "Saving…" },
            )
        } else if saved {
            (icon(Icon::Check, 14.0, p.ink(p.live)), "Saved")
        } else {
            (
                icon(
                    if scanner { Icon::Scan } else { Icon::Camera },
                    14.0,
                    if connected { p.text } else { p.tertiary },
                ),
                if self.overview() {
                    "Capture selected"
                } else if scanner {
                    "Scan"
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
            .name(label)
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
            .on_press_maybe(
                (connected && !capturing && problem.is_none()).then_some(Message::Capture),
            ),
            if capturing {
                "Saving the capture…".to_owned()
            } else if !connected {
                "Select a camera to capture from".to_owned()
            } else if let Some(problem) = problem {
                problem.to_string()
            } else {
                Action::Capture.hint("Save using the settings in Capture", os)
            },
        )
    }

    /// Whether `pending` shows its progress where it was asked for, on
    /// screen now, so the toolbar need not say it again: the control or pill
    /// that shows a command on its way is where to look for it.
    fn shown_in_place(&self, pending: &Pending) -> bool {
        match pending.job {
            // The Capture button beside the status reads "Saving…".
            Job::Capture => true,
            // The overview's pill reads "Starting…" or "Stopping…".
            Job::Start | Job::Stop if pending.batch => self.overview(),
            // The single camera's pill does, for the camera shown; a tile's
            // own button shows nothing.
            Job::Start | Job::Stop => {
                !self.overview() && pending.camera == self.snapshot.active_camera
            }
            // The camera list's spinners.
            Job::Discover | Job::Connect => self.sidebar_shown(),
            Job::NetworkScan => self.sidebar_shown() || self.network_open,
            _ => false,
        }
    }

    /// The toolbar's status: the notice, else the newest command on its
    /// way that shows nowhere else, else the camera's link.
    fn status(&self, width: f32, p: &'static Palette) -> Element<'_, Message> {
        let snapshot = &self.snapshot;
        // A long schedule must not hide a different participant's completed
        // edit or failure. Show the newest outcome before returning to work
        // that is still active in the session.
        if let Some(notice) = &self.notice
            && !self.welcome_says(notice)
            && self.client_activity.latest_changed.is_some_and(|id| {
                snapshot
                    .activity
                    .iter()
                    .any(|entry| entry.id == id && !client_activity::active(entry))
            })
        {
            return self.notice_line(notice, width, p);
        }
        if let Some(entry) = snapshot
            .activity
            .iter()
            .filter(|entry| self.other_activity(entry) && client_activity::active(entry))
            .max_by_key(|entry| {
                (
                    self.client_activity.latest_changed == Some(entry.id),
                    client_activity::effective_state(entry)
                        == crate::session::ActivityState::Running,
                    entry.id,
                )
            })
        {
            let target = entry
                .camera
                .as_ref()
                .map(|camera| format!(" · {camera}"))
                .unwrap_or_default();
            let selected_id = entry.id;
            let others = snapshot
                .activity
                .iter()
                .filter(|entry| {
                    self.other_activity(entry)
                        && client_activity::active(entry)
                        && entry.id != selected_id
                })
                .count();
            let more = if others > 0 {
                format!(" · +{others} active")
            } else {
                String::new()
            };
            let label = format!(
                "{}{} · {}: {}{}",
                client_activity::actor_text(entry),
                target,
                client_activity::activity_state(entry),
                entry.label,
                more
            );
            let visible = format!(
                "{} · {}: {}{}{}",
                client_activity::actor_display(entry),
                client_activity::activity_state(entry),
                entry.label,
                target,
                more
            );
            return accessibility::label(
                tip_above(
                    row![
                        icon::spinner(12.0, p.accent_text, self.spin()),
                        container(one_line(visible, style::SMALL, style::SANS, p.secondary))
                            .max_width((width - 19.0).max(0.0)),
                    ]
                    .spacing(7)
                    .align_y(Alignment::Center),
                    label.clone(),
                ),
                label,
                accessibility::Role::Status,
            );
        }
        if let Some(notice) = &self.notice
            && !self.welcome_says(notice)
        {
            return self.notice_line(notice, width, p);
        }
        if let Some(pending) = self
            .pending
            .iter()
            .filter(|pending| !self.shown_in_place(pending))
            .max_by_key(|pending| pending.at)
        {
            // The newest command, most likely the one just asked for.
            return row![
                dot(fade(p.accent, self.pulse()), 6.0),
                one_line(
                    format!("{}…", pending.label()),
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
        if let Some(status) = self.network_status(camera) {
            return self.network_indicator(status, p);
        }
        let mut parts = row![
            text(format!("{} · {}", camera.transport, identity(camera)))
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

    /// Whether the welcome screen's discovery callout already shows what
    /// `notice` says, so the toolbar need not say it again.
    fn welcome_says(&self, notice: &Notice) -> bool {
        let issues = &self.side.discovery_issues;
        self.shown.is_none()
            && self.snapshot.connected.is_none()
            && !self.overview()
            && !issues.is_empty()
            && notice.detail.as_deref() == Some(issues.join("\n").as_str())
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
        let level = notice.level;
        // Problems use the same semantic ink as Activity and their glyphs.
        let ink = if level == Level::Done {
            p.secondary
        } else {
            level.color(p)
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
            icon(level.icon(), 13.0, fade(level.color(p), alpha)),
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
                    .name("Show activity details")
                    .padding([2, 6])
                    .style(faded(style::link, alpha))
                    .on_press(Message::ShowActivity),
            );
        }
        if dismiss {
            line = line.push(space().width(2)).push(tip_above(
                button(icon(Icon::Close, 11.0, fade(p.secondary, alpha)))
                    .name("Dismiss notification")
                    .padding(4)
                    .style(faded(style::plain, alpha))
                    .on_press(Message::DismissNotice),
                "Dismiss",
            ));
        }
        accessibility::label(
            line.into(),
            notice.announcement(),
            if level == Level::Done {
                accessibility::Role::Status
            } else {
                accessibility::Role::Alert
            },
        )
    }

    pub(super) fn activity(&self, p: &'static Palette) -> Element<'_, Message> {
        // The Activity scrollbar overlays its content. Reserve its gutter for
        // both raw worker logs and structured client commands.
        let mut lines = column![].spacing(3).width(Fill).padding(iced::Padding {
            right: 12.0,
            ..iced::Padding::ZERO
        });
        if self.snapshot.logs.is_empty() && self.snapshot.activity.is_empty() {
            lines = lines.push(
                text("CLI, API and GUI actions will appear here, with their progress and results.")
                    .size(style::SMALL)
                    .color(p.secondary),
            );
        }
        let raw_keys = time_format::log_keys(&self.snapshot.logs);
        let current: HashSet<_> = raw_keys.iter().collect();
        let mut time_ids = self.client_activity.raw_time_ids.borrow_mut();
        time_ids.retain(|key, _| current.contains(key));
        let mut date_ids = self.client_activity.raw_date_ids.borrow_mut();
        date_ids.retain(|key, _| current.contains(key));
        let mut diagnostics = iced::widget::keyed_column([]).width(Fill).spacing(3);
        let mut previous_day = None;
        for (entry, key) in self.snapshot.logs.iter().zip(raw_keys) {
            let time_id = *time_ids
                .entry(key.clone())
                .or_insert_with(accessibility::new_node_id);
            let date_id = *date_ids
                .entry(key)
                .or_insert_with(accessibility::new_node_id);
            let labels = time_format::log(entry);
            let day = labels.day.clone();
            let boundary = previous_day != Some(day.key);
            previous_day = Some(day.key);
            // Errors and warnings read by shape as well as color, as notices do.
            let (mark, ink): (Element<'_, Message>, _) = match Level::from_log(&entry.level) {
                Some(level) => (level.mark(11.0, p), level.color(p)),
                None => (space().into(), p.text),
            };
            diagnostics = diagnostics.push(
                time_id,
                time_format::dated_row(
                    row![
                        time_format::view_with_id(labels, p, Some(time_id)),
                        container(mark)
                            .width(Length::Fixed(11.0))
                            // Centered on the first line, should the message wrap.
                            .height(Length::Fixed(style::line_height(style::CAPTION)))
                            .align_y(Alignment::Center),
                        text(entry.message.as_str())
                            .size(style::CAPTION)
                            .font(style::MONO)
                            .color(ink)
                            .width(Fill)
                            .wrapping(text::Wrapping::WordOrGlyph),
                    ]
                    .spacing(6)
                    .width(Fill)
                    .into(),
                    boundary.then_some(day),
                    "Diagnostics",
                    date_id,
                    p,
                ),
            );
        }
        lines = lines.push(diagnostics).push(self.client_activity_rows(p));
        activity_scroll(
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
                            .name("Copy activity log")
                            .padding([3, 8])
                            .style(style::link)
                            .on_press(Message::CopyLog),
                        ]
                        .align_y(Alignment::Center),
                        scrollable(lines)
                            .id(ACTIVITY_BODY)
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
            .into(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn shared_actions_are_announced_with_actor_camera_and_concurrent_work() {
        use iced::advanced::{renderer::Headless, widget::Tree};
        let renderer = iced::futures::executor::block_on(iced::Renderer::new(
            iced::Font::default(),
            iced::Pixels(16.0),
            Some("tiny-skia"),
        ))
        .expect("software renderer");
        let peer = |id, state| {
            serde_json::from_value(json!({
                "id": id, "origin": "client", "actor": {
                    "id": format!("agent-{id}"), "name": format!("Agent {id}"), "kind": "agent"
                }, "operation": "capture", "camera": "sim:0", "feature": null,
                "label": "Capture frame", "state": state, "submitted_at_ms": 1,
                "started_at_ms": 1, "finished_at_ms": null, "detail": null,
                "file_count": 0, "files": [], "job": null
            }))
            .unwrap()
        };
        let mut bench = Workbench::new(SessionHandle::new(), "multiplayer".into(), false, None);
        bench.snapshot.activity = vec![peer(1, "running"), peer(2, "running")];
        bench.sync_client_activity();
        let announcements = |bench: &Workbench| {
            let mut view = bench.status(600.0, Palette::of(false));
            let mut tree = Tree::new(view.as_widget());
            let layout = view.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &iced::advanced::layout::Limits::new(Size::ZERO, Size::new(600.0, 50.0)),
            );
            let (native, _, _) = accessibility::snapshot_widget(
                view.as_widget_mut(),
                &mut tree,
                iced::advanced::Layout::new(&layout),
                &renderer,
            );
            native
                .nodes
                .iter()
                .filter(|(_, node)| {
                    matches!(
                        node.role(),
                        accessibility::Role::Status | accessibility::Role::Alert
                    )
                })
                .map(|(_, node)| {
                    (
                        node.role(),
                        node.label().unwrap_or_default().to_owned(),
                        node.live(),
                    )
                })
                .collect::<Vec<_>>()
        };
        let active = announcements(&bench);
        assert!(active.iter().any(|(role, label, live)| {
            *role == accessibility::Role::Status
                && label.contains("Agent 2 [agent-2] · AI agent · sim:0 · Running")
                && label.contains("+1 active")
                && *live == Some(accesskit::Live::Polite)
        }));
        bench.snapshot.activity[1].state = crate::session::ActivityState::Failed;
        bench.snapshot.activity[1].detail = Some("disk full".into());
        bench.sync_client_activity();
        let completed = announcements(&bench);
        assert!(
            completed.iter().any(|(role, label, live)| {
                *role == accessibility::Role::Alert
                    && label.contains("Agent 2 [agent-2] · sim:0 · Failed")
                    && label.contains("Participant: AI agent · agent-2")
                    && label.contains("disk full")
                    && *live == Some(accesskit::Live::Assertive)
            }),
            "a failure remains visible while another participant's capture runs"
        );
    }

    #[test]
    fn native_notice_announces_outcome_with_details_and_preserves_live_identity() {
        use iced::advanced::{renderer::Headless, widget::Tree};
        let renderer = iced::futures::executor::block_on(iced::Renderer::new(
            iced::Font::default(),
            iced::Pixels(16.0),
            Some("tiny-skia"),
        ))
        .expect("software renderer");
        let bench = Workbench::new(SessionHandle::new(), "notice-test".into(), true, None);
        let now = bench.now;
        let cases = [
            (
                Job::Capture,
                Ok(json!({"files": ["/tmp/capture.png"], "count": 1})),
                "Saved capture.png\n/tmp/capture.png",
                accessibility::Role::Status,
                accesskit::Live::Polite,
            ),
            (
                Job::Capture,
                Err(anyhow::anyhow!(
                    "output file already exists\nChoose a new file name."
                )),
                "Capture failed: output file already exists\nChoose a new file name.",
                accessibility::Role::Alert,
                accesskit::Live::Assertive,
            ),
            (
                Job::Forward { recording: true },
                Err(anyhow::anyhow!("encoder unavailable")),
                "Could not start recording: encoder unavailable",
                accessibility::Role::Alert,
                accesskit::Live::Assertive,
            ),
        ];
        let mut tree = Tree::empty();
        let mut previous = None;
        for (job, result, expected, role, live) in cases {
            let (text, detail, level) = command_notice(&job, &result).unwrap();
            let notice = Notice {
                text,
                detail,
                level,
                at: now,
            };
            let mut view = bench.notice_line(&notice, 420.0, Palette::of(false));
            tree.diff(view.as_widget());
            let node = view.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &iced::advanced::layout::Limits::new(Size::ZERO, Size::new(420.0, 50.0)),
            );
            let (native, _, _) = accessibility::snapshot_widget(
                view.as_widget_mut(),
                &mut tree,
                iced::advanced::Layout::new(&node),
                &renderer,
            );
            let (id, region) = native
                .nodes
                .iter()
                .find(|(_, node)| node.role() == role)
                .unwrap();
            assert_eq!(region.label(), Some(expected));
            assert_eq!(region.live(), Some(live));
            assert!(region.is_live_atomic());
            assert!(!region.supports_action(accesskit::Action::Focus));
            assert!(!region.supports_action(accesskit::Action::Click));
            if let Some(previous) = previous {
                assert_eq!(*id, previous);
            }
            previous = Some(*id);
            assert!(
                !native
                    .nodes
                    .iter()
                    .any(|(_, node)| node.role() == accessibility::Role::Label
                        && node.label() == Some(notice.text.as_str())),
                "outcome text belongs to the live region once"
            );
            if level == Level::Error {
                for action in ["Show activity details", "Dismiss notification"] {
                    let node = native
                        .nodes
                        .iter()
                        .find(|(_, node)| node.label() == Some(action))
                        .unwrap();
                    assert!(node.1.supports_action(accesskit::Action::Click));
                }
            }
        }
    }

    #[test]
    fn progress_shows_once_where_it_was_asked_for() {
        let mut bench = Workbench::new(SessionHandle::new(), "test".into(), true, None);
        let mut other = liveness::streaming_camera(0, 0, 0.0);
        other.info.id = "sim:1".into();
        bench.snapshot.cameras = vec![liveness::streaming_camera(0, 0, 0.0), other];
        bench.snapshot.active_camera = Some("sim:0".into());
        let start = |camera: &str, batch: bool| Pending {
            camera: Some(camera.into()),
            batch,
            ..Pending::unanswered(Job::Start)
        };
        bench.focus_camera = true;
        assert!(
            bench.shown_in_place(&start("sim:0", false)),
            "the pill says it"
        );
        assert!(!bench.shown_in_place(&start("sim:1", false)));
        bench.focus_camera = false;
        assert!(
            !bench.shown_in_place(&start("sim:1", false)),
            "a tile's button shows nothing"
        );
        assert!(
            bench.shown_in_place(&start("sim:1", true)),
            "the overview pill"
        );
        let discovery = Pending::unanswered(Job::Discover);
        bench.sidebar_open = true;
        assert!(bench.shown_in_place(&discovery), "the camera list spins");
        bench.sidebar_open = false;
        assert!(!bench.shown_in_place(&discovery));
        assert!(!bench.shown_in_place(&Pending::unanswered(Job::Set("Gain".into()))));
    }

    #[test]
    fn the_welcome_callout_speaks_for_discovery_once() {
        let mut bench = Workbench::new(SessionHandle::new(), "test".into(), true, None);
        let discovery = Pending::unanswered(Job::Discover);
        let result = Ok(json!({"devices": [{}], "warnings": ["USB access denied"]}));
        bench.settle(&discovery, &result);
        let notice = bench.notice.clone().expect("a warning notice");
        assert!(bench.welcome_says(&notice), "the callout shows it");
        let other = Notice {
            text: "Capture failed: busy".into(),
            detail: Some("busy".into()),
            ..notice.clone()
        };
        assert!(!bench.welcome_says(&other), "not what the callout says");
        bench.snapshot.connected = Some(liveness::streaming_camera(0, 0, 0.0).info);
        assert!(!bench.welcome_says(&notice), "no callout once connected");
    }
}
