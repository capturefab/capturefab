//! A portable capture session and a quick, session-local feedback loop.
//! Database work and image conversion stay on the camera worker.
use super::*;
use base64::Engine as _;
use iced::widget::{column, image};
use iced::{ContentFit, Point};

const SENSITIVITY: [Choice; 3] = [
    choice("0.25", "Fewer suggestions"),
    choice("0.5", "Balanced"),
    choice("0.8", "More suggestions"),
];
const COOLDOWN: [Choice; 4] = [
    choice("1000", "1 second"),
    choice("2000", "2 seconds"),
    choice("5000", "5 seconds"),
    choice("10000", "10 seconds"),
];

#[derive(Clone, Debug)]
pub(super) enum Input {
    Setup(bool),
    Options(bool),
    New,
    Resume,
    Chosen {
        path: Option<String>,
        create: bool,
        camera: String,
    },
    Close,
    Enabled(bool),
    Sensitivity(Choice),
    Cooldown(Choice),
    Teach,
    Review(bool),
    Decide(bool),
    Retry,
    Pointer(Point),
    DragStart,
    DragEnd,
    DragCancel,
}

#[derive(Default)]
pub(super) struct State {
    pub open: bool,
    setup_open: bool,
    options_open: bool,
    dialog_busy: bool,
    last_directory: Option<PathBuf>,
    session: Option<(String, String)>,
    review_context: Option<(String, String)>,
    card: Option<Card>,
    error: Option<String>,
    last_poll: Option<Instant>,
    reviewed: u64,
    pointer: Option<Point>,
    drag_start: Option<Point>,
}

struct Card {
    id: u64,
    score: f64,
    reason: String,
    image: image::Handle,
}

impl Card {
    fn from_value(value: &serde_json::Value) -> Result<Option<Self>> {
        #[derive(serde::Deserialize)]
        struct Preview {
            id: u64,
            score: f64,
            reason: String,
            width: u32,
            height: u32,
            rgba: String,
        }
        let value = value
            .get("capture")
            .ok_or_else(|| anyhow::anyhow!("The session did not return a capture preview"))?;
        if value.is_null() {
            return Ok(None);
        }
        let preview: Preview = serde_json::from_value(value.clone())?;
        // The worker bounds previews. Validate again before allocating a GUI
        // image, including when another version of a worker is connected.
        anyhow::ensure!(
            preview.width > 0
                && preview.height > 0
                && preview.width <= 1024
                && preview.height <= 1024
                && preview.rgba.len() <= 1024 * 1024 * 6,
            "Invalid capture preview dimensions"
        );
        let rgba = base64::engine::general_purpose::STANDARD.decode(&preview.rgba)?;
        anyhow::ensure!(
            rgba.len() == preview.width as usize * preview.height as usize * 4,
            "Invalid capture preview pixels"
        );
        anyhow::ensure!(preview.score.is_finite(), "Invalid capture match score");
        Ok(Some(Self {
            id: preview.id,
            score: preview.score,
            reason: preview.reason,
            image: image::Handle::from_rgba(preview.width, preview.height, rgba),
        }))
    }
}

fn input(input: Input) -> Message {
    Message::AutoCapture(input)
}

fn state_guidance(state: &str) -> &'static str {
    match state {
        "learning" => "Capture an example to start learning.",
        "watching" => "Watching for frames like your examples.",
        "storage_full" => {
            "Storage is full. Discard suggestions, free space or raise the storage budget; a session holds up to 2 GiB."
        }
        "review_queue_full" => {
            "The review queue is full. Keep or discard suggestions to continue auto capture."
        }
        "paused" => "Auto capture is paused. Turn it on to watch the stream.",
        _ => "Capture examples and review suggestions to teach this session.",
    }
}

fn swipe_decision(start: Point, end: Point) -> Option<bool> {
    let dx = end.x - start.x;
    let dy = end.y - start.y;
    (dx.abs() >= 90.0 && dx.abs() > dy.abs() * 1.5).then_some(dx > 0.0)
}

fn key_decision(chord: Chord, captured: bool, editor: bool) -> Option<bool> {
    if captured || editor || chord.mods != keys::Mods::NONE {
        return None;
    }
    match chord.key {
        keys::Key::Left => Some(false),
        keys::Key::Right => Some(true),
        _ => None,
    }
}

impl Workbench {
    /// Screenshot scenes use a frame from the running camera and synthetic
    /// session metadata; they never create or modify a capture database.
    pub(super) fn scene_autocapture(&mut self, word: &str, late: bool) -> bool {
        if !matches!(word, "auto-capture" | "auto-capture-review") {
            return false;
        }
        self.tab = Tab::Capture;
        self.inspector_open = true;
        self.inspector_peek = !self.inspector_docked();
        if !late {
            return false;
        }
        let Some(camera) = self.snapshot.active_camera.clone() else {
            return false;
        };
        let status = crate::autocapture::SessionStatus {
            path: "/captures/line-1.duckdb".into(),
            camera: camera.clone(),
            enabled: true,
            sensitivity: 0.5,
            cooldown_ms: 2000,
            manual_count: 4,
            kept_count: 12,
            discarded_count: 3,
            pending_count: 8,
            blocked_storage: false,
            state: "watching".into(),
            model: "Session exemplar kNN + background novelty".into(),
            note: None,
        };
        if let Some(selected) = self
            .snapshot
            .cameras
            .iter_mut()
            .find(|entry| entry.info.id == camera)
        {
            selected.capture_session = Some(status.clone());
        }
        self.snapshot.capture_session = Some(status);
        self.sync_capture_session();
        if word == "auto-capture-review" {
            let Some(frame) = self.selected_frame() else {
                return false;
            };
            let Ok((width, height, rgba)) = frame::preview_rgba(&frame, 480, 360) else {
                return false;
            };
            self.auto_capture.card = Some(Card {
                id: 24,
                score: 0.87,
                reason: "Similar to your kept examples; unusual against the recent background."
                    .into(),
                image: image::Handle::from_rgba(width, height, rgba),
            });
            self.auto_capture.review_context = self.capture_session_context();
            self.auto_capture.reviewed = 3;
            self.auto_capture.open = true;
            self.sheets.shown.set(1.0);
        }
        true
    }

    fn capture_session_context(&self) -> Option<(String, String)> {
        let status = self.snapshot.capture_session.as_ref()?;
        Some((status.camera.clone(), status.path.clone()))
    }

    fn capture_session_busy(&self) -> bool {
        self.auto_capture.dialog_busy
            || [
                Job::CaptureSessionOpen,
                Job::CaptureSessionClose,
                Job::AutoCapture,
                Job::CaptureExample,
                Job::CaptureReview,
                Job::ReviewCapture,
            ]
            .into_iter()
            .any(|job| self.pending(job))
    }

    pub(super) fn update_autocapture(&mut self, event: Input) -> Task<Message> {
        match event {
            Input::Setup(open) => self.auto_capture.setup_open = open,
            Input::Options(open) => self.auto_capture.options_open = open,
            Input::New | Input::Resume => {
                if self.capture_session_busy()
                    || self.snapshot.connected.is_none()
                    || self.snapshot.capture_session.is_some()
                {
                    return Task::none();
                }
                self.auto_capture.dialog_busy = true;
                let camera = self
                    .snapshot
                    .active_camera
                    .clone()
                    .expect("connected camera");
                let create = matches!(event, Input::New);
                let directory = self
                    .snapshot
                    .capture_session
                    .as_ref()
                    .and_then(|status| std::path::Path::new(&status.path).parent())
                    .map(std::path::Path::to_owned)
                    .or_else(|| self.auto_capture.last_directory.clone());
                return Task::perform(
                    async move {
                        let mut dialog = rfd::AsyncFileDialog::new()
                            .add_filter("Capture session", &["duckdb"])
                            .set_title(if create {
                                "New capture session"
                            } else {
                                "Resume capture session"
                            });
                        if let Some(directory) = directory {
                            dialog = dialog.set_directory(directory);
                        }
                        let file = if create {
                            dialog
                                .set_file_name(format!(
                                    "capture-session-{}.duckdb",
                                    chrono::Local::now().format("%Y%m%d-%H%M%S")
                                ))
                                .save_file()
                                .await
                        } else {
                            dialog.pick_file().await
                        };
                        Input::Chosen {
                            path: file.map(|file| {
                                let mut path = file.path().to_owned();
                                if create && path.extension().is_none() {
                                    path.set_extension("duckdb");
                                }
                                path.to_string_lossy().into_owned()
                            }),
                            create,
                            camera,
                        }
                    },
                    input,
                );
            }
            Input::Chosen {
                path,
                create,
                camera,
            } => {
                self.auto_capture.dialog_busy = false;
                if let Some(path) = path {
                    self.send_to(
                        &camera,
                        Job::CaptureSessionOpen,
                        SessionCommand::CaptureSessionOpen { path, create },
                    );
                }
            }
            Input::Close if !self.capture_session_busy() => {
                self.send(
                    Job::CaptureSessionClose,
                    SessionCommand::CaptureSessionClose,
                );
            }
            Input::Enabled(enabled) if !self.capture_session_busy() => {
                if let Some(status) = &self.snapshot.capture_session {
                    self.send(
                        Job::AutoCapture,
                        SessionCommand::AutoCapture {
                            enabled,
                            sensitivity: status.sensitivity,
                            cooldown_ms: status.cooldown_ms,
                        },
                    );
                }
            }
            Input::Sensitivity(choice) if !self.capture_session_busy() => {
                if let Some(status) = &self.snapshot.capture_session {
                    self.send(
                        Job::AutoCapture,
                        SessionCommand::AutoCapture {
                            enabled: status.enabled,
                            sensitivity: choice.value.parse().unwrap_or(0.5),
                            cooldown_ms: status.cooldown_ms,
                        },
                    );
                }
            }
            Input::Cooldown(choice) if !self.capture_session_busy() => {
                if let Some(status) = &self.snapshot.capture_session {
                    self.send(
                        Job::AutoCapture,
                        SessionCommand::AutoCapture {
                            enabled: status.enabled,
                            sensitivity: status.sensitivity,
                            cooldown_ms: choice.value.parse().unwrap_or(2000),
                        },
                    );
                }
            }
            Input::Teach if !self.capture_session_busy() => {
                self.send(
                    Job::CaptureExample,
                    SessionCommand::CaptureExample {
                        timeout_ms: self.capture_timeout(None),
                    },
                );
            }
            Input::Review(open) => {
                if open && !self.auto_capture.open && self.sheet_open() {
                    return Task::none();
                }
                self.auto_capture.open = open;
                self.auto_capture.drag_start = None;
                if open {
                    self.auto_capture.error = None;
                    self.auto_capture.reviewed = 0;
                    self.fetch_capture_review();
                    return operation::focus("sheet-close");
                }
            }
            Input::Decide(keep) => self.decide_capture(keep),
            Input::Retry => {
                self.auto_capture.error = None;
                self.fetch_capture_review();
            }
            Input::Pointer(point) => self.auto_capture.pointer = Some(point),
            Input::DragStart => {
                self.auto_capture.drag_start = self.auto_capture.pointer;
            }
            Input::DragEnd => {
                let start = self.auto_capture.drag_start.take();
                if let Some((start, end)) = start.zip(self.auto_capture.pointer)
                    && let Some(keep) = swipe_decision(start, end)
                {
                    self.decide_capture(keep);
                }
            }
            Input::DragCancel => self.auto_capture.drag_start = None,
            _ => {}
        }
        Task::none()
    }

    fn fetch_capture_review(&mut self) {
        if self.snapshot.capture_session.is_none()
            || self.pending(Job::CaptureReview)
            || self.pending(Job::ReviewCapture)
        {
            return;
        }
        self.auto_capture.review_context = self.capture_session_context();
        self.auto_capture.last_poll = Some(self.now);
        self.send(
            Job::CaptureReview,
            SessionCommand::CaptureReview { id: None },
        );
    }

    fn decide_capture(&mut self, keep: bool) {
        if !self.auto_capture.open
            || self.close_error.is_some()
            || self.closing.is_some()
            || self.capture_session_busy()
            || self.auto_capture.review_context != self.capture_session_context()
        {
            return;
        }
        if let Some(card) = &self.auto_capture.card {
            self.send(
                Job::ReviewCapture,
                SessionCommand::ReviewCapture { id: card.id, keep },
            );
        }
    }

    /// Plain review keys never take cursor movement from an editor or popup.
    pub(super) fn review_shortcut(&mut self, chord: Chord, captured: bool) -> bool {
        if !self.auto_capture.open
            || self.close_error.is_some()
            || self.focused_window != self.main_window
            || self.popup_open
        {
            return false;
        }
        if let Some(keep) = key_decision(chord, captured, self.menu_editor) {
            self.decide_capture(keep);
            return true;
        }
        false
    }

    pub(super) fn tick_autocapture(&mut self) {
        self.sync_capture_session();
        if self.auto_capture.open
            && self.auto_capture.card.is_none()
            && self.auto_capture.error.is_none()
            && self
                .snapshot
                .capture_session
                .as_ref()
                .is_some_and(|status| status.pending_count > 0)
            && self
                .auto_capture
                .last_poll
                .is_none_or(|at| self.now.saturating_duration_since(at) >= Duration::from_secs(1))
        {
            self.fetch_capture_review();
        }
    }

    fn sync_capture_session(&mut self) {
        let context = self.capture_session_context();
        if context != self.auto_capture.session {
            if let Some((_, path)) = &context {
                self.auto_capture.last_directory = std::path::Path::new(path)
                    .parent()
                    .map(std::path::Path::to_owned);
            }
            self.auto_capture.session = context;
            self.auto_capture.setup_open = false;
            self.auto_capture.options_open = false;
            self.auto_capture.card = None;
            self.auto_capture.error = None;
            self.auto_capture.open = false;
            self.auto_capture.drag_start = None;
            self.auto_capture.reviewed = 0;
        }
    }

    pub(super) fn result_autocapture(
        &mut self,
        pending: &Pending,
        result: &Result<serde_json::Value>,
    ) {
        if !matches!(
            pending.job,
            Job::CaptureSessionOpen
                | Job::CaptureSessionClose
                | Job::AutoCapture
                | Job::CaptureExample
                | Job::CaptureReview
                | Job::ReviewCapture
        ) || pending.camera != self.snapshot.active_camera
        {
            return;
        }
        // Replies can arrive between slow ticks. Refresh only this camera's
        // database status so the next review cannot use an old pending count.
        let mut snapshot = self.handle.snapshot();
        snapshot.select_view(self.snapshot.active_camera.as_deref());
        self.snapshot.capture_session = snapshot.capture_session;
        self.sync_capture_session();
        if pending.job == Job::CaptureReview {
            if self.auto_capture.review_context != self.capture_session_context() {
                return;
            }
            match result {
                Ok(value) => match Card::from_value(value) {
                    Ok(card) => {
                        self.auto_capture.card = card;
                        self.auto_capture.error = None;
                    }
                    Err(error) => self.auto_capture.error = Some(error.to_string()),
                },
                Err(error) => self.auto_capture.error = Some(format!("{error:#}")),
            }
        } else if pending.job == Job::ReviewCapture {
            match result {
                Ok(_) => {
                    self.auto_capture.reviewed += 1;
                    self.auto_capture.card = None;
                    self.auto_capture.error = None;
                    self.auto_capture.drag_start = None;
                    if self.auto_capture.open {
                        self.fetch_capture_review();
                    }
                }
                Err(error) => self.auto_capture.error = Some(format!("{error:#}")),
            }
        }
    }

    pub(super) fn auto_capture_settings(&self, p: &'static Palette) -> Element<'_, Message> {
        let busy = self.capture_session_busy();
        let can_open = self.snapshot.capture_session.is_none();
        let connected = self.snapshot.connected.is_some() && !self.scanner_selected();
        let expanded = !can_open || self.auto_capture.setup_open;
        let action = |label: String, event, enabled: bool| {
            button(center(text(label).size(style::BODY)).height(Length::Shrink))
                .width(Fill)
                .padding([8, 10])
                .style(style::secondary)
                .on_press_maybe(enabled.then_some(input(event)))
        };
        let header: Element<'_, Message> = if can_open {
            disclosure(
                "Automatic capture",
                (!expanded).then(|| "Teach & review".into()),
                if expanded { 1.0 } else { 0.0 },
                input(Input::Setup(!expanded)),
                p,
            )
        } else {
            text("Automatic capture")
                .size(style::BODY)
                .font(style::BOLD)
                .into()
        };
        // Folded, it is a row like the panel's other disclosures; open, its
        // body sits in a well under the same header, which does not move.
        if !expanded {
            return header;
        }
        // Over its body in the well once a session is open; above it before.
        let (outside, inside) = if can_open {
            (Some(header), None)
        } else {
            (None, Some(header))
        };
        let mut content = column![].spacing(9);
        if let Some(header) = inside {
            content = content.push(header);
        }
        if let Some(status) = &self.snapshot.capture_session {
            let name = std::path::Path::new(&status.path)
                .file_name()
                .unwrap_or_default()
                .to_string_lossy();
            content = content
                .push(tip(
                    button(one_line(name, style::SMALL, style::MEDIUM, p.text).width(Fill))
                        .width(Fill)
                        .padding([5, 8])
                        .style(style::plain)
                        .on_press(Message::Copy(status.path.clone())),
                    format!("{}\nClick to copy session path", status.path),
                ))
                .push(checkbox_maybe(
                    "Auto capture while streaming",
                    status.enabled,
                    (!busy).then_some(|enabled| input(Input::Enabled(enabled))),
                ))
                .push(
                    text(state_guidance(&status.state))
                        .size(style::SMALL)
                        .color(
                            if status.blocked_storage
                                || matches!(
                                    status.state.as_str(),
                                    "storage_full" | "review_queue_full"
                                )
                            {
                                p.ink(p.warn)
                            } else {
                                p.secondary
                            },
                        ),
                );
            if let Some(note) = &status.note {
                content = content.push(text(note.clone()).size(style::SMALL).color(p.ink(p.warn)));
            }
            content = content
                .push(
                    text(format!(
                        "{} · {} kept · {} discarded",
                        super::format::plural(status.manual_count, "example", "examples"),
                        grouped(status.kept_count),
                        grouped(status.discarded_count),
                    ))
                    .size(style::SMALL)
                    .color(p.secondary),
                )
                .push(
                    action(
                        format!("Review suggestions ({})", grouped(status.pending_count)),
                        Input::Review(true),
                        !busy,
                    )
                    .style(if status.pending_count > 0 {
                        style::primary
                    } else {
                        style::secondary
                    }),
                )
                .push(
                    action(
                        if self.pending(Job::CaptureExample) {
                            "Learning…"
                        } else {
                            "Capture example"
                        }
                        .into(),
                        Input::Teach,
                        connected && !busy,
                    )
                    .style(if status.pending_count == 0 {
                        style::primary
                    } else {
                        style::secondary
                    }),
                )
                .push(
                    text("Saves the frame with this session. Capture & save also teaches it.")
                        .size(style::SMALL)
                        .color(p.secondary),
                );
            let sensitivity = if status.sensitivity < 0.4 {
                SENSITIVITY[0]
            } else if status.sensitivity > 0.65 {
                SENSITIVITY[2]
            } else {
                SENSITIVITY[1]
            };
            let gap = if status.cooldown_ms.is_multiple_of(1000) {
                format!("{} s", grouped(status.cooldown_ms / 1000))
            } else {
                format!("{} ms", grouped(status.cooldown_ms))
            };
            let summary = format!(
                "{} · {gap}",
                match sensitivity.value {
                    "0.25" => "Fewer",
                    "0.8" => "More",
                    _ => "Balanced",
                },
            );
            let open = self.auto_capture.options_open;
            let mut options = column![disclosure(
                "Suggestion settings",
                (!open).then_some(summary),
                if open { 1.0 } else { 0.0 },
                input(Input::Options(!open)),
                p,
            )]
            .spacing(9);
            if open {
                options = options.push(
                    row![
                        text("Suggestions").size(style::SMALL).width(85),
                        pick_list(
                            &SENSITIVITY[..],
                            Some(sensitivity),
                            |choice| input(Input::Sensitivity(choice)),
                        )
                        .name("Auto capture sensitivity")
                        .enabled(!busy)
                        .width(Fill)
                        .text_size(style::SMALL)
                        .padding([5, 7]),
                    ]
                    .spacing(8)
                    .align_y(Alignment::Center),
                )
                .push(
                    row![
                        text("Minimum gap").size(style::SMALL).width(85),
                        pick_list(
                            &COOLDOWN[..],
                            COOLDOWN.iter().copied().find(|choice| choice
                                .value
                                .parse::<u64>()
                                .ok()
                                == Some(status.cooldown_ms)),
                            |choice| input(Input::Cooldown(choice)),
                        )
                        .name("Minimum gap between auto captures")
                        .enabled(!busy)
                        .width(Fill)
                        .text_size(style::SMALL)
                        .padding([5, 7]),
                    ]
                    .spacing(8)
                    .align_y(Alignment::Center),
                )
                .push(tip(
                    text(&status.model).size(style::CAPTION).color(p.secondary),
                    "The model learns from examples and keep/discard feedback in this database.",
                ));
            }
            content = content.push(options);
        } else {
            content = content.push(
                text("Teach with captures you want. Review suggestions to improve this session.")
                    .size(style::SMALL)
                    .color(p.secondary),
            ).push(text(if connected {
                "Create a session or resume a .duckdb file. Frames, examples and reviews stay together."
            } else {
                "Connect a streaming camera, then create or resume a capture session."
            }).size(style::SMALL).color(p.secondary));
        }
        if can_open {
            content = content.push(
                row![
                    action("New session".into(), Input::New, connected && !busy),
                    action("Resume…".into(), Input::Resume, connected && !busy),
                ]
                .spacing(8),
            );
        } else {
            content = content
                .push(
                    button(text("Save & close session").size(style::SMALL))
                        .width(Fill)
                        .padding([5, 8])
                        .style(style::plain)
                        .on_press_maybe((!busy).then_some(input(Input::Close))),
                )
                .push(
                    text("Save & close to change sessions.")
                        .size(style::SMALL)
                        .color(p.secondary),
                );
        }
        let well = container(content)
            .padding(12)
            .width(Fill)
            .style(style::well);
        match outside {
            Some(header) => column![header, well]
                .spacing(super::inspector::ROW_GAP)
                .into(),
            None => well.into(),
        }
    }

    pub(super) fn auto_capture_review(
        &self,
        p: &'static Palette,
        height: f32,
    ) -> Element<'_, Message> {
        let state = &self.auto_capture;
        let pending = self
            .snapshot
            .capture_session
            .as_ref()
            .map_or(0, |status| status.pending_count);
        let busy = self.pending(Job::ReviewCapture);
        let ready = state.card.is_some() && !self.capture_session_busy();
        // This fixed body slot owns the review's live outcome. The toolbar
        // is excluded behind the modal, so its notices cannot announce errors
        // or completions here. Counts and drag movement are deliberately not
        // part of this value: only review transitions change it.
        let (outcome, outcome_role) = if let Some(error) = &state.error {
            (
                format!("Capture review failed: {error}"),
                accessibility::Role::Alert,
            )
        } else if busy {
            ("Saving your decision…".into(), accessibility::Role::Status)
        } else if self.pending(Job::CaptureReview) {
            ("Loading next capture…".into(), accessibility::Role::Status)
        } else if let Some(card) = &state.card {
            (
                format!(
                    "{}Capture {} ready. Swipe or use ← / →.",
                    if state.reviewed > 0 {
                        "Decision saved. "
                    } else {
                        ""
                    },
                    card.id,
                ),
                accessibility::Role::Status,
            )
        } else {
            (
                if state.reviewed > 0 {
                    "Decision saved. You're caught up."
                } else {
                    "You're caught up."
                }
                .into(),
                accessibility::Role::Status,
            )
        };
        let footer = column![
            row![
                button(
                    center(
                        row![
                            icon(Icon::Close, 20.0, p.ink(p.danger)),
                            text("Discard  ←").size(style::BODY).font(style::BOLD)
                        ]
                        .spacing(9)
                    )
                    .height(Length::Shrink)
                )
                .id("capture-discard")
                .name("Discard capture, left arrow")
                .padding([13, 18])
                .height(48)
                .width(Fill)
                .style(style::danger)
                .on_press_maybe(ready.then_some(input(Input::Decide(false)))),
                button(
                    center(
                        row![
                            icon(Icon::Check, 20.0, Color::WHITE),
                            text("Keep  →").size(style::BODY).font(style::BOLD)
                        ]
                        .spacing(9)
                    )
                    .height(Length::Shrink)
                )
                .id("capture-keep")
                .name("Keep capture, right arrow")
                .padding([13, 18])
                .height(48)
                .width(Fill)
                .style(style::primary)
                .on_press_maybe(ready.then_some(input(Input::Decide(true)))),
            ]
            .spacing(14),
            text(if busy {
                "Saving your decision…"
            } else {
                "Swipe the image or use ← / →. Every decision teaches this session."
            })
            .size(style::SMALL)
            .color(p.secondary),
        ]
        .spacing(10);
        let mut body = column![
            text(format!(
                "{} waiting · {} reviewed",
                grouped(pending),
                grouped(state.reviewed)
            ))
            .size(style::BODY)
            .color(p.secondary),
            accessibility::label(
                text(outcome.clone())
                    .size(style::SMALL)
                    .color(if state.error.is_some() {
                        p.ink(p.danger)
                    } else {
                        p.secondary
                    })
                    .width(Fill)
                    .wrapping(text::Wrapping::WordOrGlyph)
                    .into(),
                outcome,
                outcome_role,
            ),
        ]
        .spacing(14);
        if let Some(card) = &state.card {
            let swipe = state
                .drag_start
                .zip(state.pointer)
                .and_then(|(start, end)| swipe_decision(start, end));
            let caption = match swipe {
                Some(true) => "Release to keep →",
                Some(false) => "← Release to discard",
                None => "← Discard                         Keep →",
            };
            body = body
                .push(
                    mouse_area(
                        container(accessibility::label_description(
                            image(card.image.clone())
                                .content_fit(ContentFit::Contain)
                                .width(Fill)
                                .height((height - 370.0).clamp(130.0, 480.0))
                                .into(),
                            format!("Suggested capture {} preview", card.id),
                            "Preview of the capture being reviewed. Use Keep or Discard, or the right or left arrow keys, to record your decision.".into(),
                            accessibility::Role::Image,
                            None,
                        ))
                        .id("capture-review-image")
                        .padding(6)
                        .width(Fill)
                        .style(style::code),
                    )
                    .on_move(|point| input(Input::Pointer(point)))
                    .on_press(input(Input::DragStart))
                    .on_release(input(Input::DragEnd))
                    .on_exit(input(Input::DragCancel)),
                )
                .push(
                    text(caption)
                        .size(style::SMALL)
                        .font(style::MEDIUM)
                        .color(match swipe {
                            Some(true) => p.ink(p.live),
                            Some(false) => p.ink(p.danger),
                            None => p.secondary,
                        }),
                )
                .push(
                    text(format!(
                        "Capture {} · match score {:.2}",
                        card.id, card.score
                    ))
                    .size(style::SMALL)
                    .font(style::MEDIUM),
                )
                .push(text(&card.reason).size(style::SMALL).color(p.secondary));
        } else {
            body = body.push(container(column![
                icon(if state.error.is_some() { Icon::Close } else if self.pending(Job::CaptureReview) { Icon::Recent } else { Icon::Check }, 42.0, p.accent_text),
                text(if state.error.is_some() { "Review unavailable" } else if self.pending(Job::CaptureReview) { "Loading next capture…" } else { "You're caught up" })
                    .size(style::DISPLAY).font(style::BOLD),
                text(if state.error.is_some() { "Try again to load this session's pending captures." } else { "Leave auto capture streaming, then come back to review more suggestions." })
                    .size(style::BODY).color(p.secondary),
            ].spacing(14)).width(Fill).padding([32, 16]));
        }
        // Keep the optional retry inside its own slot, rather than adding or
        // removing siblings that can replace other widgets' native state.
        let mut retry = column![].width(Fill);
        if state.error.is_some() {
            retry = retry.push(
                button(text("Try again").size(style::BODY))
                    .padding([6, 12])
                    .style(style::secondary)
                    .on_press(input(Input::Retry)),
            );
        }
        body = body.push(retry);
        sheet_frame("Review auto captures", input(Input::Review(false)), body, p)
            .mark(Icon::Camera)
            .footer(footer)
            .size((self.width - 56.0).clamp(0.0, 920.0), height)
            .into()
    }
}

#[cfg(test)]
#[path = "autocapture/accessibility_tests.rs"]
mod accessibility_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn preview_validates_pixels_before_creating_image() {
        let good = json!({"capture": {"id": 1, "score": 0.7, "reason": "Similar to example", "width": 1, "height": 1, "rgba": "/////w=="}});
        assert!(Card::from_value(&good).unwrap().is_some());
        let mut bad = good.clone();
        bad["capture"]["width"] = json!(2);
        assert!(Card::from_value(&bad).is_err());
        bad["capture"]["width"] = json!(u32::MAX);
        assert!(Card::from_value(&bad).is_err());
        assert!(
            Card::from_value(&json!({"capture": null}))
                .unwrap()
                .is_none()
        );
        assert!(Card::from_value(&json!({})).is_err());
    }

    #[test]
    fn arrows_only_review_with_a_free_keyboard() {
        let chord = Chord {
            key: keys::Key::Left,
            mods: keys::Mods::NONE,
        };
        assert_eq!(key_decision(chord, false, false), Some(false));
        assert_eq!(
            key_decision(
                Chord {
                    key: keys::Key::Right,
                    ..chord
                },
                false,
                false
            ),
            Some(true)
        );
        assert_eq!(key_decision(chord, true, false), None);
        assert_eq!(key_decision(chord, false, true), None);
        assert_eq!(
            key_decision(
                Chord {
                    mods: keys::Mods {
                        command: true,
                        ..keys::Mods::NONE
                    },
                    ..chord
                },
                false,
                false
            ),
            None
        );
    }

    #[test]
    fn swipes_require_deliberate_horizontal_movement() {
        let start = Point::new(200.0, 200.0);
        assert_eq!(swipe_decision(start, Point::new(300.0, 210.0)), Some(true));
        assert_eq!(swipe_decision(start, Point::new(100.0, 210.0)), Some(false));
        assert_eq!(swipe_decision(start, Point::new(220.0, 200.0)), None);
        assert_eq!(swipe_decision(start, Point::new(300.0, 300.0)), None);
    }

    #[test]
    fn changing_sessions_closes_review_and_clears_the_previous_card() {
        let mut app = Workbench::new(SessionHandle::new(), "test".into(), true, None);
        app.snapshot.capture_session = Some(crate::autocapture::SessionStatus {
            path: "/tmp/first.duckdb".into(),
            camera: "sim:0".into(),
            enabled: true,
            sensitivity: 0.5,
            cooldown_ms: 2000,
            manual_count: 1,
            kept_count: 0,
            discarded_count: 0,
            pending_count: 1,
            blocked_storage: false,
            state: "Learning".into(),
            model: "kNN".into(),
            note: None,
        });
        app.sync_capture_session();
        app.auto_capture.open = true;
        app.auto_capture.card = Some(Card {
            id: 1,
            score: 0.7,
            reason: "Example".into(),
            image: image::Handle::from_rgba(1, 1, vec![0; 4]),
        });
        app.snapshot.capture_session.as_mut().unwrap().path = "/tmp/second.duckdb".into();
        app.tick_autocapture();
        assert!(!app.auto_capture.open);
        assert!(app.auto_capture.card.is_none());
        assert_eq!(app.auto_capture.last_directory, Some(PathBuf::from("/tmp")));
    }

    #[test]
    fn native_close_dismisses_review_before_closing_the_workbench() {
        let mut app = Workbench::new(SessionHandle::new(), "test".into(), true, None);
        app.auto_capture.open = true;
        let _ = app.update(Message::CloseRequested(app.main_window));
        assert!(!app.auto_capture.open);
        assert!(app.closing.is_none());
    }

    #[test]
    fn status_guidance_explains_learning_and_capacity_limits() {
        assert!(state_guidance("learning").contains("Capture an example"));
        assert!(state_guidance("watching").contains("Watching"));
        assert!(state_guidance("storage_full").contains("Discard suggestions"));
        assert!(state_guidance("review_queue_full").contains("Keep or discard"));
        assert!(state_guidance("paused").contains("paused"));
    }

    #[test]
    fn review_keeps_the_image_reason_and_compact_decisions_visible() {
        use iced::advanced::{
            Layout, layout,
            widget::{Id, Operation, Tree, operation::scrollable},
        };
        use iced::{Rectangle, Size, Vector};

        #[derive(Default)]
        struct Geometry {
            decisions: Vec<Rectangle>,
            image: Option<Rectangle>,
            score: Option<Rectangle>,
            reason: Option<Rectangle>,
            scroller: Option<(Rectangle, Rectangle)>,
        }
        impl Operation for Geometry {
            fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
                operate(self);
            }
            fn container(&mut self, id: Option<&Id>, bounds: Rectangle) {
                if id == Some(&Id::new("capture-review-image")) {
                    self.image = Some(bounds);
                }
            }
            fn custom(&mut self, _: Option<&Id>, bounds: Rectangle, state: &mut dyn std::any::Any) {
                if let Some(control) = state.downcast_ref::<accessibility::Control<Message>>()
                    && matches!(
                        control.activate.as_ref(),
                        Some(Message::AutoCapture(Input::Decide(_)))
                    )
                {
                    self.decisions.push(bounds);
                }
            }
            fn text(&mut self, _: Option<&Id>, bounds: Rectangle, value: &str) {
                if value.starts_with("Capture 24 · match score") {
                    self.score = Some(bounds);
                }
                if value.starts_with("Similar to your kept examples") {
                    self.reason = Some(bounds);
                }
            }
            fn scrollable(
                &mut self,
                id: Option<&Id>,
                bounds: Rectangle,
                content: Rectangle,
                _: Vector,
                _: &mut dyn scrollable::Scrollable,
            ) {
                if id == Some(&Id::new(SHEET_BODY)) {
                    self.scroller = Some((bounds, content));
                }
            }
        }
        fn inside(inner: Rectangle, outer: Rectangle) -> bool {
            inner.width > 0.0
                && inner.height > 0.0
                && inner.x >= outer.x - 0.5
                && inner.y >= outer.y - 0.5
                && inner.x + inner.width <= outer.x + outer.width + 0.5
                && inner.y + inner.height <= outer.y + outer.height + 0.5
        }

        let renderer = crate::gui::destinations::layout_tests::renderer();
        for size in [Size::new(900.0, 620.0), Size::new(1300.0, 853.0)] {
            let mut app = Workbench::new(SessionHandle::new(), "review-layout".into(), true, None);
            app.width = size.width;
            app.height = size.height;
            app.auto_capture.open = true;
            app.auto_capture.card = Some(Card {
                id: 24,
                score: 0.87,
                reason: "Similar to your kept examples; unusual against the recent background."
                    .into(),
                image: image::Handle::from_rgba(480, 360, vec![127; 480 * 360 * 4]),
            });
            let mut view = app.auto_capture_review(Palette::of(false), size.height - 80.0);
            let mut tree = Tree::new(view.as_widget());
            let node = view.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, size),
            );
            let mut geometry = Geometry::default();
            view.as_widget_mut()
                .operate(&mut tree, Layout::new(&node), &renderer, &mut geometry);
            assert_eq!(geometry.decisions.len(), 2);
            for bounds in geometry.decisions {
                assert!(
                    (40.0..70.0).contains(&bounds.height),
                    "compact review decision at {size:?}: {bounds:?}"
                );
                assert!(inside(bounds, Rectangle::with_size(size)));
            }
            let (viewport, content) = geometry.scroller.expect("review body viewport");
            assert!(
                content.height <= viewport.height + 1.0,
                "review fits without scrolling at {size:?}: viewport={viewport:?}, content={content:?}"
            );
            for bounds in [
                geometry.image.expect("review image"),
                geometry.score.expect("match score"),
                geometry.reason.expect("capture reason"),
            ] {
                assert!(
                    inside(bounds, viewport),
                    "review content stays visible at {size:?}: {bounds:?} within {viewport:?}"
                );
            }
        }
    }
}
