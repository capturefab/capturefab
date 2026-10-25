//! The bottom bar and the activity log above it.
use super::*;
use iced::widget::column;
use std::collections::HashSet;
#[cfg(test)]
mod date_layout_tests;
#[cfg(test)]
mod raw_log_tests;
#[cfg(test)]
mod responsive_layout_tests;

/// The widest the status at the right of the toolbar grows.
const STATUS: f32 = 420.0;
/// Keep the widest Capture label readable.
const CAPTURE_ROOM: f32 = 120.0;
/// Room the Capture All button takes as its glyph, and the gap before it.
const CAPTURE_ALL_ROOM: f32 = 46.0;
/// What Capture All's words add.
const CAPTURE_ALL_LABEL: f32 = 84.0;
/// Room the Record button takes as its glyph, and the gap before it.
const RECORD_ROOM: f32 = 44.0;
/// What Record's word adds.
const RECORD_LABEL: f32 = 52.0;
/// Room the elapsed-time pill beside a recording's Stop button takes.
const PILL_ROOM: f32 = 60.0;
/// Room that pill takes when it counts down a timed recording, "1:02:03 left".
const TIMED_PILL_ROOM: f32 = 112.0;
/// A compact notice still keeps its details and dismiss actions visible.
const MIN_STATUS: f32 = 128.0;
/// The room a warning or error is given before the toolbar's extras: its
/// actions and enough text to say what went wrong.
const PROBLEM_STATUS: f32 = NOTICE_GLYPH + NOTICE_DETAILS + NOTICE_DISMISS + 110.0;
/// Narrower than this, a notice's text gives way to its glyph alone.
const NOTICE_TEXT_MIN: f32 = 60.0;

struct ToolbarFit {
    history: bool,
    activity_label: bool,
    analytics_label: bool,
    resources_label: bool,
    destination_label: bool,
    /// The destination shows at all; the docked sidebar also shows it.
    destination: bool,
    /// Capture All keeps its words.
    capture_all_label: bool,
    /// Record keeps its word.
    record_label: bool,
    status: f32,
}
/// Room a notice's glyph and the gap after it take.
const NOTICE_GLYPH: f32 = 13.0 + 6.0;
/// Room a notice's "Details" link and the gap before it take.
const NOTICE_DETAILS: f32 = 52.0 + 4.0;
/// Room a notice's dismiss button and the gap before it take.
const NOTICE_DISMISS: f32 = 21.0 + 2.0;

/// The folder a relative output name is written under: the process's
/// working directory, which the worker inherits (nothing resolves a
/// relative name against the capture root), with the home folder folded to
/// `~` the way `Workbench::output_preview` folds it.
fn shown_base() -> String {
    let Ok(cwd) = std::env::current_dir() else {
        return "the current folder".into();
    };
    fold_home(&cwd, prefs::capture_root().parent())
}

/// `path` with a leading `home` folded to `~`.
pub(super) fn fold_home(path: &std::path::Path, home: Option<&std::path::Path>) -> String {
    match home
        .filter(|home| !home.as_os_str().is_empty())
        .and_then(|home| path.strip_prefix(home).ok())
    {
        Some(rest) if rest.as_os_str().is_empty() => "~".into(),
        Some(rest) => format!("~{}{}", std::path::MAIN_SEPARATOR, rest.display()),
        None => path.display().to_string(),
    }
}

/// Whole seconds on a clock face: "04:07", "1:02:03".
fn clock_face(seconds: u64) -> String {
    if seconds >= 3600 {
        format!(
            "{}:{:02}:{:02}",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        )
    } else {
        format!("{:02}:{:02}", seconds / 60, seconds % 60)
    }
}

/// What the pill beside a recording's Stop button reads at `now`: how long
/// it has recorded since `since`, or, when the session stops it by itself at
/// `until`, the time left, rounded up so it reaches zero only at the end.
/// UTC Unix milliseconds.
fn recording_clock(now: u64, since: u64, until: Option<u64>) -> String {
    match until {
        Some(until) => format!(
            "{} left",
            clock_face(until.saturating_sub(now).div_ceil(1000))
        ),
        None => clock_face(now.saturating_sub(since) / 1000),
    }
}

/// A running recording's progress in a line, "01:24 · 2,520 frames written",
/// adding what it lost, and whether that line is a warning: frames dropped
/// or the encoder stalled. Counters of another recording are not this one's.
pub(super) fn recording_stats(
    now: u64,
    since: Option<u64>,
    progress: Option<&crate::session::ForwardingProgress>,
    forwarding_id: Option<&str>,
) -> Option<(String, bool)> {
    let progress = progress.filter(|progress| Some(progress.id.as_str()) == forwarding_id);
    let mut parts = Vec::new();
    if let Some(since) = since {
        parts.push(clock_face(now.saturating_sub(since) / 1000));
    }
    let mut warn = false;
    if let Some(progress) = progress {
        // `dropped` includes the frames lost while the encoder was busy.
        let dropped = progress.dropped.max(progress.late);
        if progress.stalled {
            parts.push("Encoder stalled".to_owned());
            warn = true;
        } else if dropped > 0 {
            // Short, so what was lost still fits beside it.
            parts.push(format!("{} written", grouped(progress.written)));
        } else {
            parts.push(format!(
                "{} {} written",
                grouped(progress.written),
                if progress.written == 1 {
                    "frame"
                } else {
                    "frames"
                }
            ));
        }
        if dropped > 0 {
            parts.push(format!("{} dropped", grouped(dropped)));
            warn = true;
        }
    }
    (!parts.is_empty()).then(|| (parts.join(" · "), warn))
}

/// The tooltip and accessible description of a timed recording's pill: how
/// long it records and when, in the viewer's local time, it stops by itself.
fn recording_ends(since: u64, until: u64) -> String {
    let length = crate::scheduling::format_duration(until.saturating_sub(since));
    match time_format::local_time(until) {
        Some(end) => format!("Records for {length}; stops by itself at {end}"),
        None => format!("Records for {length}, then stops by itself"),
    }
}

/// Whether a previewed output name is relative, so it lands under the
/// working directory rather than where it reads.
fn is_relative(preview: &str) -> bool {
    let bytes = preview.as_bytes();
    !(preview.starts_with(['~', '/', '\\'])
        || (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'))
}

/// Where the next capture goes, in a few plain words, for the toolbar: the
/// destination's name and the first folder of the file it writes, else
/// the folder of `preview` (a path with home folded to `~`). A relative
/// name is written under the working directory, so it reads as under
/// `base`, never under a folder it does not go to. The full next file name
/// is in its tooltip.
fn save_summary(preview: &str, destination: Option<&str>, base: &str) -> String {
    let separators = ['/', '\\'];
    let parent = preview
        .rfind(separators)
        .map(|end| &preview[..end])
        .filter(|parent| !parent.is_empty());
    match destination {
        Some(name) => {
            let first =
                parent.and_then(|parent| parent.split(separators).find(|part| !part.is_empty()));
            match first {
                Some(folder) => format!("Saves to {name} · {folder}"),
                None => format!("Saves to {name}"),
            }
        }
        None if is_relative(preview) => match parent {
            Some(parent) => format!(
                "Saves to {}{}{parent}",
                base.trim_end_matches(separators),
                std::path::MAIN_SEPARATOR
            ),
            None => format!("Saves to {base}"),
        },
        None => format!("Saves to {}", parent.unwrap_or(base)),
    }
}

/// Whether a link's transport counters say something worth reading at a
/// glance: notes, resent packets or lost frames. Clean counters wait in a
/// tooltip.
fn transport_inline(stats: &TransportStats) -> bool {
    !stats.notes.is_empty() || stats.resend_requested > 0 || format::transport_loss(stats) > 0
}

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
        let capture = snapshot.connected.is_some();
        let fit = self.toolbar_fit(capture);
        let width = fit.status;
        let mut left = row![].spacing(8).align_y(Alignment::Center);
        if capture {
            let preview = self.output_preview();
            let destination: Element<'_, Message> = if fit.destination_label {
                row![
                    icon(Icon::Folder, 14.0, p.secondary),
                    one_line(
                        save_summary(&preview, self.capture_to.selected.as_deref(), &shown_base()),
                        style::SMALL,
                        style::SANS,
                        p.secondary,
                    )
                    // The dated folder at its end says the most.
                    .keep_ends()
                    .yield_room(),
                ]
                .spacing(6)
                .align_y(Alignment::Center)
                .into()
            } else {
                icon(Icon::Folder, 16.0, p.secondary)
            };
            left = left
                .push(self.capture_button(p))
                .push(self.capture_all_button(fit.capture_all_label, p));
            if let Some(record) = self.record_button(fit.record_label, p) {
                left = left.push(record);
            }
            left = left.push(fit.destination.then(|| {
                tip_above(
                    button(destination)
                        .name(format!(
                            "Capture destination: {}",
                            self.capture_to.label(&preview)
                        ))
                        .padding([4, 6])
                        .style(style::plain)
                        .on_press(Message::ShowTab(Tab::Capture)),
                    Action::CaptureTab.hint(
                        &format!(
                            "Next file: {} · change it in Capture",
                            self.capture_to.label(&preview)
                        ),
                        os,
                    ),
                )
            }));
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
        let mut activity_content = row![icon(
            Icon::Activity,
            16.0,
            if self.logs_open {
                p.accent_text
            } else {
                p.secondary
            },
        )]
        .spacing(6)
        .align_y(Alignment::Center);
        if fit.activity_label {
            activity_content = activity_content.push(text("Activity").size(style::SMALL));
        }
        let activity_about = match unseen {
            Some(Level::Error) => "Session activity: new errors",
            Some(_) => "Session activity: new warnings",
            None if self.client_activity.unread => {
                "Session activity: new actions from other participants"
            }
            None => "Session activity",
        };
        let mut activity = stack![tip_above(
            button(activity_content)
                .name("Session activity")
                .description(activity_about)
                .expanded(self.logs_open)
                .padding(6)
                .style(style::plain)
                .on_press(Message::ToggleActivity),
            Action::ToggleActivity.hint(activity_about, os),
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
        let analytics: Element<'_, Message> = if fit.analytics_label {
            text("Analytics").size(style::SMALL).into()
        } else {
            icon(Icon::Chart, 16.0, p.secondary)
        };
        // Edit retains both actions and their shortcuts when the toolbar gives
        // its room to capture and session feedback. Keep the group in its own
        // slot so resizing does not transfer its focus to a different action.
        let history = if fit.history {
            row![
                history_button(true),
                history_button(false),
                space().width(8),
            ]
            .spacing(2)
        } else {
            row![]
        };
        let session_actions = row![
            history,
            tip_above(
                button(icon(Icon::Sparkle, 15.0, p.accent))
                    .name("Quick command")
                    .padding([4, 6])
                    .style(style::plain)
                    .on_press(Message::Assistant(assistant::Input::Quick(
                        assistant::quick::Input::Open(!self.assistant.quick.open)
                    ))),
                Action::QuickCommand.hint("Quick command", Os::CURRENT),
            ),
            space().width(8),
            self.resources_indicator(fit.resources_label, p),
            space().width(8),
            tip_above(
                button(analytics)
                    .name("Session analytics for all cameras")
                    .padding([4, 6])
                    .style(style::plain)
                    .on_press(Message::Analytics(true)),
                "Session analytics: frame rate, payload throughput and missed frames",
            ),
            space().width(10),
            activity,
        ]
        .align_y(Alignment::Center);
        let controls = row![
            container(left).width(Fill).clip(true),
            container(self.status(width, p)).max_width(width).clip(true),
            session_actions,
        ]
        .spacing(10)
        .align_y(Alignment::Center);
        column![
            rule::horizontal(1).style(style::line),
            container(controls)
                .height(TOOLBAR - 1.0)
                .align_y(Alignment::Center)
                .padding([0.0, GUTTER - 6.0])
                .clip(true),
        ]
        .into()
    }

    /// Give the capture and status controls priority as panes reduce the main
    /// area. Labels return before the duplicated Edit controls do.
    fn toolbar_fit(&self, capture: bool) -> ToolbarFit {
        let main = self.main_width();
        let history = main >= 900.0
            && (capture
                || self.history_label(true).is_some()
                || self.history_label(false).is_some());
        let record = capture && self.record_shown();
        let mut fit = ToolbarFit {
            history,
            activity_label: main >= 620.0,
            analytics_label: main >= 520.0,
            resources_label: main >= 740.0,
            destination_label: main >= 700.0,
            destination: true,
            capture_all_label: true,
            record_label: true,
            status: 0.0,
        };
        let status = |fit: &ToolbarFit| {
            let capture_room = if capture {
                CAPTURE_ROOM
                    + CAPTURE_ALL_ROOM
                    + if fit.capture_all_label {
                        CAPTURE_ALL_LABEL
                    } else {
                        0.0
                    }
                    + match (fit.destination, fit.destination_label) {
                        (false, _) => 0.0,
                        (true, false) => 36.0,
                        (true, true) => 128.0,
                    }
            } else {
                0.0
            } + if record {
                RECORD_ROOM
                    + if fit.record_label { RECORD_LABEL } else { 0.0 }
                    + self.recording_pill_room()
            } else {
                0.0
            };
            let actions = if fit.resources_label { 108.0 } else { 40.0 }
                + if fit.analytics_label { 76.0 } else { 28.0 }
                + 36.0
                + if fit.activity_label { 80.0 } else { 28.0 }
                + if fit.history { 104.0 } else { 0.0 };
            let gaps = if fit.history { 5.0 } else { 4.0 } * 10.0;
            main - 2.0 * (GUTTER - 6.0) - gaps - actions - capture_room
        };
        // A warning or error must say what went wrong: the extras give way,
        // one at a time, until its text has room to read.
        if self.problem_notice_shown() {
            let steps: [fn(&mut ToolbarFit); 6] = [
                |fit| fit.resources_label = false,
                |fit| fit.destination_label = false,
                |fit| fit.capture_all_label = false,
                |fit| fit.record_label = false,
                |fit| fit.activity_label = false,
                |fit| fit.analytics_label = false,
            ];
            for step in steps {
                if status(&fit) >= PROBLEM_STATUS {
                    break;
                }
                step(&mut fit);
            }
        }
        // With both panels docked, labels fold to their glyphs in the same
        // order, Capture All's last, so nothing overlaps. Without a notice the
        // status needs only room for a few words, unless the destination's
        // label can yield it more.
        let least = if self.problem_notice_shown() && main >= 600.0 {
            PROBLEM_STATUS
        } else if self.notice.is_some() {
            MIN_STATUS
        } else {
            NOTICE_TEXT_MIN
        };
        let steps: [fn(&mut ToolbarFit); 7] = [
            |fit| fit.resources_label = false,
            |fit| fit.destination_label = false,
            |fit| fit.activity_label = false,
            |fit| fit.analytics_label = false,
            |fit| fit.record_label = false,
            |fit| fit.destination = false,
            |fit| fit.capture_all_label = false,
        ];
        for step in steps {
            if status(&fit) >= least {
                break;
            }
            step(&mut fit);
        }
        let least = if fit.destination_label {
            MIN_STATUS
        } else {
            least
        };
        fit.status = status(&fit).clamp(least, STATUS);
        fit
    }

    /// Whether the status shows a warning or an error notice.
    fn problem_notice_shown(&self) -> bool {
        self.notice
            .as_ref()
            .is_some_and(|notice| notice.level != Level::Done && !self.welcome_says(notice))
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
        let (glyph, label): (Element<'_, Message>, &str) = if capturing {
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
                if scanner { "Scan" } else { "Capture" },
            )
        };
        tip_above(
            button(
                row![
                    glyph,
                    container(
                        // As the button's style inks it: dimmed only while
                        // disabled, not while saving.
                        one_line(
                            label,
                            style::BODY,
                            style::SANS,
                            if capturing || (connected && problem.is_none()) {
                                p.text
                            } else {
                                p.secondary
                            },
                        )
                    )
                    .max_width(CAPTURE_ROOM)
                ]
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
                Action::Capture.hint("Capture from this camera", os)
            },
        )
    }

    /// Capture All, beside Capture: one frame from every camera in the
    /// session at once. While that batch saves, the spinner takes its glyph.
    fn capture_all_button(&self, labelled: bool, p: &'static Palette) -> Element<'_, Message> {
        let problem = self.validate_input(&self.capture_command(false)).err();
        let capturing = self
            .pending
            .iter()
            .any(|pending| pending.batch && pending.job == Job::Capture);
        let ready = !capturing && problem.is_none();
        let glyph: Element<'_, Message> = if capturing {
            icon::spinner(14.0, p.secondary, self.spin())
        } else {
            icon(Icon::Grid, 14.0, if ready { p.text } else { p.tertiary })
        };
        let mut content = row![glyph].spacing(7).align_y(Alignment::Center);
        if labelled {
            content = content.push(one_line(
                "Capture All",
                style::BODY,
                style::SANS,
                if capturing || ready {
                    p.text
                } else {
                    p.secondary
                },
            ));
        }
        tip_above(
            button(content)
                .name("Capture All")
                .padding([6, 12])
                .style(move |theme, status| {
                    let status = if capturing {
                        button::Status::Active
                    } else {
                        status
                    };
                    style::secondary(theme, status)
                })
                .on_press_maybe(ready.then_some(Message::AllCapture)),
            if capturing {
                "Capturing from every camera…".to_owned()
            } else if let Some(problem) = problem {
                problem.to_string()
            } else {
                Action::CaptureAll.hint(
                    "Capture one frame from every connected camera at once",
                    Os::CURRENT,
                )
            },
        )
    }

    /// The file the selected camera records to, when this window started it.
    pub(super) fn recording_file(&self) -> Option<&str> {
        let (camera, path) = self.last_recording.as_ref()?;
        (self.recording_here() && *camera == self.snapshot.active_camera).then_some(path.as_str())
    }

    /// Whether the selected camera can record: a camera, not a scanner.
    fn record_shown(&self) -> bool {
        self.snapshot.connected.is_some() && !self.scanner_selected()
    }

    /// Record or Stop for the selected camera, with how long it has been
    /// recording beside it, or how long it has left when it has a duration.
    fn record_button(&self, labelled: bool, p: &'static Palette) -> Option<Element<'_, Message>> {
        if !self.record_shown() {
            return None;
        }
        let os = Os::CURRENT;
        let recording = self.recording_here();
        let pending = self.forward_pending();
        // A network forward runs from the Video panel; it is stopped there.
        let forwarding = self.snapshot.forwarding.is_some() && !recording;
        let (glyph, label): (Element<'_, Message>, _) = match pending {
            Some(stopping) => (
                icon::spinner(14.0, p.secondary, self.spin()),
                if stopping {
                    "Stopping…"
                } else {
                    "Starting…"
                },
            ),
            None if recording => (icon(Icon::Stop, 14.0, p.text), "Stop"),
            None => (
                icon(
                    Icon::Video,
                    14.0,
                    if forwarding { p.tertiary } else { p.text },
                ),
                "Record",
            ),
        };
        let mut content = row![tip_above(
            button(
                row![glyph]
                    .push(labelled.then(|| text(label).size(style::BODY)))
                    .spacing(7)
                    .align_y(Alignment::Center),
            )
            .name(if recording {
                "Stop recording"
            } else {
                "Record"
            })
            .padding([6, 12])
            .style(if recording {
                style::stop
            } else {
                style::secondary
            })
            .on_press_maybe((pending.is_none() && !forwarding).then_some(Message::Record)),
            if forwarding {
                "Stop forwarding in the Video panel to record".to_owned()
            } else if recording {
                Action::Record.hint("Stop and finish the recording", os)
            } else {
                Action::Record.hint("Record video to a file on this computer", os)
            },
        )]
        .spacing(6)
        .align_y(Alignment::Center);
        if recording && let Some(since) = self.snapshot.forwarding_since_ms {
            let until = self.snapshot.forwarding_until_ms;
            let clock = recording_clock(epoch_ms(), since, until);
            let pill: Element<'_, Message> = container(
                text(clock.clone())
                    .font(style::MONO)
                    .size(style::SMALL)
                    .color(p.ink(p.danger)),
            )
            .padding([2, 8])
            .style(style::pill(p.danger))
            .into();
            content = content.push(match until {
                // A timed recording also says when it stops by itself.
                Some(until) => {
                    let ends = recording_ends(since, until);
                    tip_above(
                        accessibility::label_description(
                            pill,
                            clock,
                            ends.clone(),
                            accessibility::Role::Label,
                            None,
                        ),
                        ends,
                    )
                }
                None => pill,
            });
        }
        Some(content.into())
    }

    /// Room the pill beside the selected camera's recording takes, if any.
    fn recording_pill_room(&self) -> f32 {
        if !self.recording_here() {
            0.0
        } else if self.snapshot.forwarding_until_ms.is_some() {
            TIMED_PILL_ROOM
        } else {
            PILL_ROOM
        }
    }

    /// Whether `pending` shows its progress where it was asked for, on
    /// screen now, so the toolbar need not say it again: the control or pill
    /// that shows a command on its way is where to look for it.
    fn shown_in_place(&self, pending: &Pending) -> bool {
        match pending.job {
            // The Capture button beside the status reads "Saving…".
            Job::Capture => true,
            // The camera's pill does, for the camera shown.
            Job::Start | Job::Stop => {
                !pending.batch && pending.camera == self.snapshot.active_camera
            }
            // The welcome screen's spinners.
            Job::Discover | Job::Connect => self.snapshot.connected.is_none(),
            Job::NetworkScan => self.snapshot.connected.is_none() || self.network_open,
            _ => false,
        }
    }

    /// The toolbar's status: the notice, else the newest command on its
    /// way that shows nowhere else, else the camera's link.
    fn status(&self, width: f32, p: &'static Palette) -> Element<'_, Message> {
        if let Some(status) = self.assistant_toolbar_status(p) {
            return status;
        }
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
        if let Some(stats) = snapshot.transport.as_ref().filter(|s| !transport_inline(s)) {
            // Clean counters: the link alone, with them in its tooltip.
            return tip_above(
                text(format!("{} · {}", camera.transport, identity(camera)))
                    .size(style::SMALL)
                    .color(p.secondary)
                    .wrapping(text::Wrapping::None),
                format!(
                    "{}\nPacket size · resent packets recovered/requested · incomplete or missing frames",
                    transport_text(stats)
                ),
            );
        }
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
        let glyph = icon(level.icon(), 13.0, fade(level.color(p), alpha));
        // Too narrow for words that say anything: the glyph alone, the whole
        // text on hover and in the accessible label, and Details beside it.
        let mut line = if room < NOTICE_TEXT_MIN {
            row![tip_above(glyph, notice.about())]
        } else {
            row![
                glyph,
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
        }
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
    fn recording_stats_say_how_far_it_got_and_what_it_lost() {
        let progress = |written, dropped, stalled| crate::session::ForwardingProgress {
            id: "a".into(),
            accepted: written,
            written,
            dropped,
            late: 0,
            stalled,
        };
        let stats = |progress: &crate::session::ForwardingProgress, id| {
            recording_stats(85_000, Some(1_000), Some(progress), Some(id))
        };
        assert_eq!(
            stats(&progress(2_520, 0, false), "a"),
            Some(("01:24 \u{b7} 2,520 frames written".into(), false))
        );
        assert_eq!(
            stats(&progress(1, 0, false), "a"),
            Some(("01:24 \u{b7} 1 frame written".into(), false))
        );
        assert_eq!(
            stats(&progress(2_520, 3, false), "a"),
            Some(("01:24 \u{b7} 2,520 written \u{b7} 3 dropped".into(), true))
        );
        assert_eq!(
            stats(&progress(10, 0, true), "a"),
            Some(("01:24 \u{b7} Encoder stalled".into(), true))
        );
        // Another recording's counters are not shown as this one's.
        assert_eq!(
            stats(&progress(2_520, 3, false), "b"),
            Some(("01:24".into(), false))
        );
        assert_eq!(recording_stats(0, None, None, None), None);
    }

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
        assert!(
            bench.shown_in_place(&start("sim:0", false)),
            "the pill says it"
        );
        assert!(!bench.shown_in_place(&start("sim:1", false)));
        assert!(
            !bench.shown_in_place(&start("sim:0", true)),
            "Start all shows nowhere else"
        );
        let discovery = Pending::unanswered(Job::Discover);
        assert!(bench.shown_in_place(&discovery), "the welcome screen spins");
        bench.snapshot.connected = Some(crate::transport::simulator::info());
        assert!(!bench.shown_in_place(&discovery));
        assert!(!bench.shown_in_place(&Pending::unanswered(Job::Set("Gain".into()))));
    }

    #[test]
    fn the_save_summary_says_where_in_plain_words() {
        let root = "~/work";
        // The default: a dated folder under the capture root, home folded.
        assert_eq!(
            save_summary("~/Capturefab/2026-10-17/cam-####.png", None, root),
            "Saves to ~/Capturefab/2026-10-17"
        );
        assert_eq!(
            save_summary("/mnt/data/runs/cam-####.png", None, root),
            "Saves to /mnt/data/runs"
        );
        // A relative name goes under the working directory, not the
        // capture root.
        assert_eq!(save_summary("cam-####.png", None, root), "Saves to ~/work");
        let sep = std::path::MAIN_SEPARATOR;
        assert_eq!(
            save_summary("runs/cam-####.png", None, root),
            format!("Saves to ~/work{sep}runs")
        );
        // A named destination, with the first folder of its relative name.
        assert_eq!(
            save_summary("cam-####.png", Some("Lab NAS"), root),
            "Saves to Lab NAS"
        );
        assert_eq!(
            save_summary("2026-10-17/line/cam-####.png", Some("archive"), root),
            "Saves to archive · 2026-10-17"
        );
        // Windows separators.
        assert_eq!(
            save_summary(r"~\Capturefab\2026-10-17\cam-####.png", None, r"~\work"),
            r"Saves to ~\Capturefab\2026-10-17"
        );
        assert_eq!(
            save_summary(r"C:\Captures\cam-####.png", None, root),
            r"Saves to C:\Captures"
        );
        assert_eq!(
            save_summary(r"day\cam-####.png", Some("share"), root),
            "Saves to share · day"
        );
        // Never a sequence number: that is the tooltip's, as ####.
        assert!(!save_summary("cam-####.png", None, root).contains('#'));
    }

    #[test]
    fn the_working_folder_is_shown_with_home_folded() {
        use std::path::Path;
        let home = Path::new("/Users/me");
        let sep = std::path::MAIN_SEPARATOR;
        assert_eq!(fold_home(Path::new("/Users/me"), Some(home)), "~");
        assert_eq!(
            fold_home(&home.join("work"), Some(home)),
            format!("~{sep}work")
        );
        assert_eq!(fold_home(Path::new("/srv/x"), Some(home)), "/srv/x");
        assert_eq!(fold_home(Path::new("/srv/x"), None), "/srv/x");
        assert!(is_relative("cam.png") && is_relative("runs/cam.png"));
        for absolute in ["~/a.png", "/a.png", r"\\nas\a.png", r"C:\a.png", "c:/a.png"] {
            assert!(!is_relative(absolute), "{absolute}");
        }
    }

    #[test]
    fn clean_transport_counters_wait_in_a_tooltip() {
        let clean = TransportStats {
            packet_size: Some(8192),
            packets_received: 10_000,
            ..TransportStats::default()
        };
        assert!(!transport_inline(&clean));
        let noted = TransportStats {
            notes: vec!["packet size is above the link's MTU".into()],
            ..clean.clone()
        };
        assert!(transport_inline(&noted));
        let resent = TransportStats {
            resend_requested: 3,
            resend_recovered: 3,
            ..clean.clone()
        };
        assert!(transport_inline(&resent));
        let lost = TransportStats {
            lost_frames: 1,
            ..clean.clone()
        };
        assert!(transport_inline(&lost));
        let incomplete = TransportStats {
            incomplete_frames: 2,
            ..clean
        };
        assert!(transport_inline(&incomplete));
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

    #[test]
    fn recordings_show_time_recorded_or_time_left() {
        let since = 1_000_000;
        assert_eq!(recording_clock(since + 247_900, since, None), "04:07");
        assert_eq!(recording_clock(since + 3_723_000, since, None), "1:02:03");
        let hour = Some(since + 3_600_000);
        assert_eq!(recording_clock(since, since, hour), "1:00:00 left");
        // Rounded up: it reads zero only once the end time comes.
        assert_eq!(
            recording_clock(since + 3_599_001, since, hour),
            "00:01 left"
        );
        assert_eq!(
            recording_clock(since + 3_600_000, since, hour),
            "00:00 left"
        );
        assert_eq!(
            recording_clock(since + 9_000_000, since, hour),
            "00:00 left"
        );
        assert!(recording_ends(since, since + 5_400_000).starts_with("Records for 1 h 30 min"));
    }
}
