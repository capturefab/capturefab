//! Saved camera setups: save the selected camera's settings by name, and
//! restore one after previewing what it changes. The camera worker reads
//! and compares through the session; the setup store is read and written
//! off the UI thread.
use super::*;
use crate::setup::{self as store, Reading, Summary};
use iced::widget::column;
use serde_json::Value;

const NAME_FIELD: &str = "setup-name";
const SHEET_WIDTH: f32 = 620.0;

/// The page the sheet shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Page {
    /// Saved setups, to choose one to restore.
    #[default]
    List,
    Save,
    /// The chosen setup compared with the camera.
    Restore,
}

/// How a save in the store ended.
#[derive(Clone, Debug)]
pub(super) enum Saved {
    Done {
        name: String,
        values: usize,
    },
    /// The name is taken; `Save(true)` replaces it.
    Taken(String),
    Failed(String),
}

#[derive(Clone, Debug)]
pub(super) enum Input {
    /// Open on a page, or close with `None`.
    Open(Option<Page>),
    /// Go to a page within the open sheet.
    Show(Page),
    Scrolled(bool),
    Listed(Result<(PathBuf, Vec<Summary>), String>),
    Name(String),
    Note(String),
    /// Save the reading, replacing a setup with the same name when true.
    Save(bool),
    Saved(Saved),
    /// Compare the named setup with the camera.
    Choose(String),
    AllowOtherModel(bool),
    /// The primary button: restore, or ask to confirm disruptive changes.
    Restore,
    ConfirmDelete(bool),
    Delete,
    Deleted(Result<String, String>),
    Skipped,
}

#[derive(Default)]
pub(super) struct State {
    pub(super) open: bool,
    page: Page,
    scrolled: bool,
    /// Scenes show fixtures and never touch the session or the store.
    scene: bool,
    /// The camera the sheet acts on, pinned when it opened.
    camera: Option<String>,
    dir: Option<PathBuf>,
    list: Option<Result<Vec<Summary>, String>>,
    listing: bool,
    // Save page.
    reading: Option<Result<Reading, String>>,
    reading_key: Option<u64>,
    name: String,
    note: String,
    name_error: Option<String>,
    taken: bool,
    saving: bool,
    skipped_open: bool,
    // Restore page.
    chosen: Option<String>,
    diff: Option<Result<Value, String>>,
    diff_key: Option<u64>,
    allow_other_model: bool,
    confirm: bool,
    confirm_delete: bool,
    deleting: bool,
    error: Option<String>,
}

impl State {
    fn summary(&self, name: &str) -> Option<&Summary> {
        self.list
            .as_ref()?
            .as_ref()
            .ok()?
            .iter()
            .find(|summary| summary.name.eq_ignore_ascii_case(name))
    }

    fn diff(&self) -> Option<&Value> {
        self.diff.as_ref()?.as_ref().ok()
    }
}

/// Run `work` on its own thread; `failed` stands in when it cannot run.
fn off_thread<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
    failed: impl FnOnce(String) -> T + Send + 'static,
    done: fn(T) -> Input,
) -> Task<Message> {
    Task::perform(
        async move {
            let (send, receive) = iced::futures::channel::oneshot::channel();
            let started = std::thread::Builder::new()
                .name("capturefab-setups".into())
                .spawn(move || {
                    let _ = send.send(work());
                });
            match started {
                Ok(_) => match receive.await {
                    Ok(value) => value,
                    Err(_) => failed("The setup worker stopped.".into()),
                },
                Err(error) => failed(format!("Could not start the setup worker: {error}")),
            }
        },
        move |value| Message::Setups(done(value)),
    )
}

fn list_setups() -> Task<Message> {
    off_thread(
        || {
            let store = store::Store::session();
            store
                .list()
                .map(|list| (store.dir().to_owned(), list))
                .map_err(|error| format!("{error:#}"))
        },
        Err,
        Input::Listed,
    )
}

/// "Basler acA1920-40gm", or the model alone when it names the vendor.
fn camera_label(vendor: &str, model: &str) -> String {
    match (vendor.trim(), model.trim()) {
        ("", "") => "an unknown camera".into(),
        ("", model) => model.into(),
        (vendor, "") => vendor.into(),
        (vendor, model) if model.starts_with(vendor) => model.into(),
        (vendor, model) => format!("{vendor} {model}"),
    }
}

/// "Oct 25", with the year when it is not this one.
fn saved_on(saved_at: &str) -> String {
    let Ok(at) = chrono::DateTime::parse_from_rfc3339(saved_at) else {
        return saved_at.get(..10).unwrap_or(saved_at).to_owned();
    };
    let at = at.with_timezone(&chrono::Local);
    if chrono::Datelike::year(&at) == chrono::Datelike::year(&chrono::Local::now()) {
        at.format("%b %-d").to_string()
    } else {
        at.format("%b %-d, %Y").to_string()
    }
}

/// " (GainSelector All)" for a value at a selected bank.
fn banks(row: &Value) -> String {
    let at = row["at"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|bank| {
            format!(
                "{} {}",
                bank["selector"].as_str().unwrap_or_default(),
                bank["value"].as_str().unwrap_or_default()
            )
        })
        .collect::<Vec<_>>();
    if at.is_empty() {
        String::new()
    } else {
        format!("({})", at.join(", "))
    }
}

fn shown(value: &Value) -> String {
    match value {
        Value::Null => "—".into(),
        Value::String(text) => text.clone(),
        value => value_text(value),
    }
}

fn changes(diff: &Value) -> impl Iterator<Item = &Value> {
    diff["changes"].as_array().into_iter().flatten()
}

fn disruptive(diff: &Value) -> bool {
    changes(diff).any(|change| change["risk"] == "disruptive")
}

/// The names of changes that restart the stream, once each.
fn locked(diff: &Value) -> Vec<&str> {
    let mut names = Vec::new();
    for change in changes(diff).filter(|change| change["stream_locked"] == true) {
        if let Some(name) = change["feature"].as_str()
            && !names.contains(&name)
        {
            names.push(name);
        }
    }
    names
}

/// "Width and PixelFormat", "Width, Height and PixelFormat".
fn and_list(names: &[&str]) -> String {
    match names {
        [] => String::new(),
        [one] => (*one).into(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

impl Workbench {
    /// What a reading or comparison depends on: the camera, its undoable
    /// changes, whether it streams and whether anything holds its stream.
    fn setup_key(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        self.setups.camera.hash(&mut hash);
        if let Some(camera) = self
            .snapshot
            .cameras
            .iter()
            .find(|camera| Some(&camera.info.id) == self.setups.camera.as_ref())
        {
            camera.streaming.hash(&mut hash);
            camera.forwarding.is_some().hash(&mut hash);
            camera
                .jobs
                .iter()
                .filter(|job| job.active())
                .count()
                .hash(&mut hash);
            camera.auto.is_some().hash(&mut hash);
            for entry in &camera.history.entries {
                (&entry.token, entry.applied, entry.last_changed_at_us).hash(&mut hash);
            }
        }
        hash.finish()
    }

    fn setup_camera_info(&self) -> Option<&CameraInfo> {
        let id = self.setups.camera.as_ref()?;
        self.snapshot
            .cameras
            .iter()
            .find(|camera| &camera.info.id == id)
            .map(|camera| &camera.info)
    }

    fn read_setup(&mut self) {
        let Some(camera) = self.setups.camera.clone() else {
            return;
        };
        self.setups.reading_key = Some(self.setup_key());
        if let Err(error) =
            self.try_submit(Some(&camera), Job::ReadSetup, SessionCommand::ReadSetup)
        {
            self.setups.reading = Some(Err(format!("{error:#}")));
        }
    }

    fn compare_setup(&mut self) {
        let (Some(camera), Some(name)) = (self.setups.camera.clone(), self.setups.chosen.clone())
        else {
            return;
        };
        self.setups.diff_key = Some(self.setup_key());
        self.setups.confirm = false;
        // The preview always shows what both cameras share; restoring to
        // another model still waits for the box below.
        if let Err(error) = self.try_submit(
            Some(&camera),
            Job::SetupDiff,
            SessionCommand::SetupDiff {
                name: Some(name),
                setup: None,
                allow_other_model: true,
            },
        ) {
            self.setups.diff = Some(Err(format!("{error:#}")));
        }
    }

    pub(super) fn setups_input(&mut self, input: Input) -> Task<Message> {
        let state = &mut self.setups;
        match input {
            Input::Open(Some(page)) => {
                if !state.open && self.settings_open {
                    return self
                        .handle_message(Message::Settings(false))
                        .chain(Task::done(Message::Setups(Input::Open(Some(page)))));
                }
                if !self.setups.open && self.sheet_open() {
                    return Task::none();
                }
                let Some(camera) = self.snapshot.active_camera.clone() else {
                    return Task::none();
                };
                self.setups = State {
                    open: true,
                    camera: Some(camera),
                    ..State::default()
                };
                return self.setups_page(page);
            }
            Input::Open(None) => {
                if !state.scene {
                    *state = State::default();
                }
                state.open = false;
            }
            Input::Show(page) => return self.setups_page(page),
            Input::Scrolled(scrolled) => state.scrolled = scrolled,
            Input::Listed(result) => {
                state.listing = false;
                match result {
                    Ok((dir, list)) => {
                        state.dir = Some(dir);
                        state.list = Some(Ok(list));
                    }
                    Err(error) => state.list = Some(Err(error)),
                }
            }
            Input::Name(name) => {
                state.name = name;
                state.name_error = None;
                state.taken = false;
            }
            Input::Note(note) => state.note = note.chars().take(store::MAX_NOTE).collect(),
            Input::Save(replace) => {
                let name = state.name.trim().to_owned();
                if let Err(error) = store::validate_name(&name) {
                    state.name_error = Some(error.to_string());
                    return Task::none();
                }
                let Some(Ok(reading)) = state.reading.clone() else {
                    return Task::none();
                };
                if state.saving || state.scene {
                    return Task::none();
                }
                state.saving = true;
                state.error = None;
                let note = Some(state.note.trim().to_owned()).filter(|note| !note.is_empty());
                return off_thread(
                    move || {
                        let setup = match store::from_read(&name, note, reading) {
                            Ok(setup) => setup,
                            Err(error) => return Saved::Failed(format!("{error:#}")),
                        };
                        match store::Store::session().save(&setup, replace) {
                            Ok(_) => Saved::Done {
                                name: setup.name,
                                values: setup.values.len(),
                            },
                            Err(error) if error.is::<store::NameTaken>() => {
                                Saved::Taken(error.to_string())
                            }
                            Err(error) => Saved::Failed(format!("{error:#}")),
                        }
                    },
                    Saved::Failed,
                    Input::Saved,
                );
            }
            Input::Saved(saved) => {
                state.saving = false;
                match saved {
                    Saved::Done { name, values } => {
                        *state = State::default();
                        self.set_notice(
                            format!(
                                "Saved setup {name} ({})",
                                plural(values as u64, "setting", "settings")
                            ),
                            None,
                            Level::Done,
                        );
                    }
                    Saved::Taken(message) => {
                        state.taken = true;
                        state.name_error = Some(message.replace(
                            "; pass --replace to overwrite it",
                            ". Replace it, or choose another name.",
                        ));
                    }
                    Saved::Failed(error) => state.error = Some(error),
                }
            }
            Input::Choose(name) => {
                state.chosen = Some(name);
                state.diff = None;
                state.allow_other_model = false;
                state.confirm_delete = false;
                state.error = None;
                return self.setups_page(Page::Restore);
            }
            Input::AllowOtherModel(allow) => {
                state.allow_other_model = allow;
                state.confirm = false;
            }
            Input::Restore => return self.restore_setup(),
            Input::ConfirmDelete(confirm) => state.confirm_delete = confirm,
            Input::Delete => {
                let Some(name) = state.chosen.clone() else {
                    return Task::none();
                };
                if state.deleting || state.scene {
                    return Task::none();
                }
                state.deleting = true;
                return off_thread(
                    move || {
                        store::Store::session()
                            .remove(&name)
                            .map(|removed| removed.name)
                            .map_err(|error| format!("{error:#}"))
                    },
                    Err,
                    Input::Deleted,
                );
            }
            Input::Deleted(result) => {
                state.deleting = false;
                state.confirm_delete = false;
                match result {
                    Ok(name) => {
                        self.set_notice(format!("Deleted setup {name}"), None, Level::Done);
                        return self.setups_page(Page::List);
                    }
                    Err(error) => state.error = Some(error),
                }
            }
            Input::Skipped => state.skipped_open = !state.skipped_open,
        }
        Task::none()
    }

    fn setups_page(&mut self, page: Page) -> Task<Message> {
        let state = &mut self.setups;
        state.page = page;
        state.scrolled = false;
        state.confirm = false;
        state.confirm_delete = false;
        let mut tasks = vec![operation::snap_to(
            SHEET_BODY,
            operation::RelativeOffset::START,
        )];
        if state.scene {
            return Task::batch(tasks);
        }
        match page {
            Page::List => {
                state.chosen = None;
                state.diff = None;
                if !state.listing {
                    state.listing = true;
                    tasks.push(list_setups());
                }
                tasks.push(operation::focus("sheet-close"));
            }
            Page::Save => {
                state.reading = None;
                self.read_setup();
                tasks.push(operation::focus(NAME_FIELD));
            }
            Page::Restore => {
                if state.list.is_none() && !state.listing {
                    state.listing = true;
                    tasks.push(list_setups());
                }
                self.compare_setup();
                tasks.push(operation::focus("sheet-close"));
            }
        }
        Task::batch(tasks)
    }

    fn restore_setup(&mut self) -> Task<Message> {
        let Some(diff) = self.setups.diff().cloned() else {
            return Task::none();
        };
        if !self.can_restore_setup(&diff) {
            return Task::none();
        }
        if disruptive(&diff) && !self.setups.confirm {
            self.setups.confirm = true;
            return Task::none();
        }
        let (Some(camera), Some(name)) = (self.setups.camera.clone(), self.setups.chosen.clone())
        else {
            return Task::none();
        };
        self.setups.confirm = false;
        self.setups.error = None;
        if self.setups.scene {
            return Task::none();
        }
        let allow_other_model = diff["match"] == "other_model";
        if let Err(error) = self.try_submit(
            Some(&camera),
            Job::ApplySetup,
            SessionCommand::ApplySetup {
                name: Some(name),
                setup: None,
                digest: diff["digest"].as_str().map(str::to_owned),
                allow_other_model,
                strict: false,
            },
        ) {
            self.setups.error = Some(format!("{error:#}"));
        }
        Task::none()
    }

    /// Whether the primary button can restore what the preview shows.
    fn can_restore_setup(&self, diff: &Value) -> bool {
        changes(diff).next().is_some()
            && diff["blocked"].is_null()
            && (diff["match"] != "other_model" || self.setups.allow_other_model)
            && !self.pending(Job::ApplySetup)
            && !self.pending(Job::SetupDiff)
    }

    /// A reading, comparison or restore came back.
    pub(super) fn setups_result(&mut self, pending: &Pending, result: &Result<Value>) {
        if !matches!(
            pending.job,
            Job::ReadSetup | Job::SetupDiff | Job::ApplySetup
        ) || pending.camera != self.setups.camera
            || !self.setups.open
        {
            return;
        }
        let outcome = match result {
            Ok(value) => Ok(value.clone()),
            Err(error) => Err(format!("{error:#}")),
        };
        match pending.job {
            Job::ReadSetup if self.setups.page == Page::Save => {
                self.setups.reading = Some(outcome.and_then(|value| {
                    serde_json::from_value(value).map_err(|error| error.to_string())
                }));
            }
            Job::SetupDiff if self.setups.page == Page::Restore => {
                self.setups.diff = Some(outcome);
            }
            Job::ApplySetup => match outcome {
                Ok(_) => {
                    self.setups = State::default();
                }
                Err(error) => {
                    self.setups.error = Some(error);
                    self.compare_setup();
                }
            },
            _ => {}
        }
    }

    /// Read or compare again after the camera changed while the sheet is open.
    pub(super) fn tick_setups(&mut self) {
        if !self.setups.open || self.setups.scene {
            return;
        }
        if self
            .setups
            .camera
            .as_ref()
            .is_none_or(|id| !self.snapshot.cameras.iter().any(|c| &c.info.id == id))
        {
            self.setups = State::default();
            return;
        }
        let key = Some(self.setup_key());
        match self.setups.page {
            Page::Save
                if self.setups.reading_key != key
                    && !self.setups.saving
                    && !self.pending(Job::ReadSetup) =>
            {
                self.read_setup();
            }
            Page::Restore
                if self.setups.diff_key != key
                    && !self.pending(Job::SetupDiff)
                    && !self.pending(Job::ApplySetup) =>
            {
                self.compare_setup();
            }
            _ => {}
        }
    }

    pub(super) fn setups_sheet(&self, p: &'static Palette, height: f32) -> Element<'_, Message> {
        let state = &self.setups;
        let (title, body, footer) = match state.page {
            Page::List => (
                "Restore Setup",
                self.setup_list(p),
                self.setup_list_footer(p),
            ),
            Page::Save => ("Save Setup", self.setup_save(p), self.setup_save_footer(p)),
            Page::Restore => (
                "Restore Setup",
                self.setup_restore(p),
                self.setup_restore_footer(p),
            ),
        };
        let title = match (state.page, &state.chosen) {
            (Page::Restore, Some(name)) => format!("Restore {name}"),
            _ => title.into(),
        };
        sheet_frame(title, Message::Setups(Input::Open(None)), body, p)
            .mark(Icon::Sliders)
            .back((state.page == Page::Restore).then_some(Message::Setups(Input::Show(Page::List))))
            .size(SHEET_WIDTH.min((self.width - 48.0).max(0.0)), height)
            .footer(footer)
            // Restoring needs a deliberate click: Return never applies.
            .default_action(None)
            .scrolled(state.scrolled)
            .on_scroll(|value| Message::Setups(Input::Scrolled(value)))
            .into()
    }

    fn setup_list(&self, p: &'static Palette) -> Element<'_, Message> {
        let state = &self.setups;
        let mut body = column![
            text(
                "Choose a setup to compare with this camera. Nothing changes until you restore it."
            )
            .size(style::BODY)
            .color(p.secondary)
        ]
        .spacing(12);
        if let Some(error) = &state.error {
            body = body.push(field_error(error.clone(), p));
        }
        match &state.list {
            None => {
                body = body.push(
                    row![
                        icon::spinner(14.0, p.secondary, self.spin()),
                        text("Reading saved setups…")
                            .size(style::SMALL)
                            .color(p.secondary)
                    ]
                    .spacing(8)
                    .align_y(Alignment::Center),
                );
            }
            Some(Err(error)) => {
                body = body.push(field_error(format!("Saved setups unavailable: {error}"), p));
            }
            Some(Ok(list)) if list.is_empty() => {
                body = body.push(
                    text("No saved setups yet. Save this camera's settings to restore them later.")
                        .size(style::BODY)
                        .color(p.text),
                );
            }
            Some(Ok(list)) => {
                let live = self.setup_camera_info();
                let mut rows = column![].spacing(6);
                for summary in list {
                    let other = live.is_some_and(|live| {
                        !(summary
                            .vendor
                            .trim()
                            .eq_ignore_ascii_case(live.vendor.trim())
                            && summary.model.trim().eq_ignore_ascii_case(live.model.trim()))
                    });
                    let mut detail = vec![
                        camera_label(&summary.vendor, &summary.model),
                        format!("saved {}", saved_on(&summary.saved_at)),
                        plural(summary.values as u64, "setting", "settings"),
                    ];
                    if other {
                        detail.push("another model".into());
                    }
                    let detail = detail.join(" · ");
                    let mut info = column![
                        text(summary.name.as_str())
                            .size(style::BODY)
                            .font(style::SEMIBOLD)
                            .color(p.text),
                        one_line(detail.clone(), style::SMALL, style::SANS, p.secondary),
                    ]
                    .spacing(2);
                    if let Some(note) = &summary.note {
                        info = info.push(one_line(
                            note.clone(),
                            style::SMALL,
                            style::SANS,
                            p.secondary,
                        ));
                    }
                    rows = rows.push(
                        button(
                            row![
                                info.width(Fill),
                                icon(Icon::ChevronRight, 12.0, p.secondary)
                            ]
                            .spacing(8)
                            .align_y(Alignment::Center),
                        )
                        .name(format!("Compare setup {}", summary.name))
                        .description(detail)
                        .width(Fill)
                        .padding([8, 10])
                        .style(style::card)
                        .on_press(Message::Setups(Input::Choose(summary.name.clone()))),
                    );
                }
                body = body.push(rows);
            }
        }
        body.into()
    }

    fn setup_list_footer(&self, p: &'static Palette) -> Element<'_, Message> {
        let folder = self
            .setups
            .dir
            .as_ref()
            .map(|dir| format!("Saved in {}", destinations::home_short(dir)))
            .unwrap_or_default();
        row![
            text(folder)
                .size(style::CAPTION)
                .color(p.secondary)
                .width(Fill),
            button(text("Save Current Settings…").size(style::BODY))
                .name("Save current settings as a setup")
                .padding([7, 14])
                .style(style::secondary)
                .on_press_maybe(
                    (!self.history_busy()).then_some(Message::Setups(Input::Show(Page::Save)))
                ),
        ]
        .spacing(12)
        .align_y(Alignment::Center)
        .into()
    }

    fn setup_save(&self, p: &'static Palette) -> Element<'_, Message> {
        let state = &self.setups;
        let editable = !state.saving;
        let mut name = text_input("e.g. line-2-mono", &state.name)
            .id(NAME_FIELD)
            .name("Setup name")
            .invalid(state.name_error.clone())
            .on_input_maybe(editable.then_some(|value| Message::Setups(Input::Name(value))))
            .on_submit_maybe(
                self.can_save_setup()
                    .then_some(Message::Setups(Input::Save(false))),
            )
            .size(style::BODY)
            .padding([6, 10]);
        if state.name_error.is_some() {
            name = name.style(style::input_invalid);
        }
        let note = text_input("Note (optional)", &state.note)
            .name("Setup note")
            .on_input_maybe(editable.then_some(|value| Message::Setups(Input::Note(value))))
            .size(style::BODY)
            .padding([6, 10]);
        let mut body = column![].spacing(12);
        match &state.reading {
            None => {
                body = body.push(
                    row![
                        icon::spinner(14.0, p.secondary, self.spin()),
                        text("Reading the camera's settings…")
                            .size(style::BODY)
                            .color(p.secondary)
                    ]
                    .spacing(8)
                    .align_y(Alignment::Center),
                );
            }
            Some(Err(error)) => {
                body = body.push(field_error(
                    format!("Could not read the camera's settings: {error}"),
                    p,
                ));
            }
            Some(Ok(reading)) => {
                let camera = &reading.camera;
                let from = match &camera.serial {
                    Some(serial) => format!(
                        "{} (serial {serial})",
                        camera_label(&camera.vendor, &camera.model)
                    ),
                    None => camera_label(&camera.vendor, &camera.model),
                };
                let mut summary = format!(
                    "Saves {} from {from}. Network, user-set and file settings are not saved.",
                    plural(reading.values.len() as u64, "setting", "settings")
                );
                if let Some(auto) = &reading.auto {
                    summary.push_str(&format!(
                        " Auto exposure is on; restoring turns it on with balance {:.2}.",
                        auto.balance
                    ));
                }
                body = body.push(text(summary).size(style::BODY).color(p.text));
            }
        }
        let mut name_group = column![name].spacing(5);
        if let Some(error) = &state.name_error {
            let mut line = row![field_error(error.clone(), p)]
                .spacing(8)
                .align_y(Alignment::Center);
            if state.taken {
                line = line.push(
                    button(text("Replace").size(style::SMALL))
                        .name(format!("Replace setup {}", state.name.trim()))
                        .padding([3, 10])
                        .style(style::secondary)
                        .on_press_maybe(
                            self.can_save_setup()
                                .then_some(Message::Setups(Input::Save(true))),
                        ),
                );
            }
            name_group = name_group.push(line);
        }
        body = body.push(name_group).push(note);
        if let Some(error) = &state.error {
            body = body.push(field_error(error.clone(), p));
        }
        if let Some(Ok(reading)) = &state.reading
            && !reading.skipped.is_empty()
        {
            let mut skipped = column![disclosure(
                format!("{} not saved", reading.skipped.len()),
                None,
                if state.skipped_open { 1.0 } else { 0.0 },
                Message::Setups(Input::Skipped),
                p,
            )]
            .spacing(6);
            if state.skipped_open {
                for row in &reading.skipped {
                    let at = row
                        .at
                        .iter()
                        .map(|bank| bank.value.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    let name = if at.is_empty() {
                        row.feature.clone()
                    } else {
                        format!("{} at {at}", row.feature)
                    };
                    skipped = skipped.push(
                        text(format!("{name}: {}", row.reason))
                            .size(style::SMALL)
                            .color(p.secondary),
                    );
                }
            }
            body = body.push(skipped);
        }
        body.into()
    }

    fn can_save_setup(&self) -> bool {
        let state = &self.setups;
        !state.saving && !state.name.trim().is_empty() && matches!(state.reading, Some(Ok(_)))
    }

    fn setup_save_footer(&self, _p: &'static Palette) -> Element<'_, Message> {
        let cancel = button(text("Cancel").size(style::BODY))
            .padding([7, 14])
            .style(style::secondary)
            .on_press(Message::Setups(Input::Open(None)));
        let save = button(
            text(if self.setups.saving {
                "Saving…"
            } else {
                "Save"
            })
            .size(style::BODY),
        )
        .name("Save setup")
        .padding([7, 18])
        .style(style::primary)
        .on_press_maybe(
            self.can_save_setup()
                .then_some(Message::Setups(Input::Save(false))),
        );
        let (first, second) = dialog_actions(cancel, save, false);
        row![space::horizontal(), first, second]
            .spacing(8)
            .align_y(Alignment::Center)
            .into()
    }

    fn setup_restore(&self, p: &'static Palette) -> Element<'_, Message> {
        let state = &self.setups;
        let mut body = column![].spacing(14);
        if let Some(error) = &state.error {
            body = body.push(field_error(
                format!("Could not restore the setup: {error}"),
                p,
            ));
        }
        let diff = match &state.diff {
            None => {
                return body
                    .push(
                        row![
                            icon::spinner(14.0, p.secondary, self.spin()),
                            text("Comparing with the camera…")
                                .size(style::BODY)
                                .color(p.secondary)
                        ]
                        .spacing(8)
                        .align_y(Alignment::Center),
                    )
                    .into();
            }
            Some(Err(error)) => {
                return body.push(field_error(error.clone(), p)).into();
            }
            Some(Ok(diff)) => diff,
        };
        let live = diff["camera"].clone();
        let live_label = camera_label(
            live["vendor"].as_str().unwrap_or_default(),
            live["model"].as_str().unwrap_or_default(),
        );
        let warnings = diff["warnings"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(" ");
        match diff["match"].as_str() {
            Some("other_model") => {
                let saved = state
                    .chosen
                    .as_deref()
                    .and_then(|name| state.summary(name))
                    .map_or_else(
                        || "another camera model".to_owned(),
                        |summary| camera_label(&summary.vendor, &summary.model),
                    );
                body = body.push(banner(
                    true,
                    format!(
                        "Saved from {saved}. This camera is {live_label}; only settings both share will change."
                    ),
                    p,
                ));
                body = body.push(checkbox(
                    "Apply to this different camera model",
                    state.allow_other_model,
                    |allow| Message::Setups(Input::AllowOtherModel(allow)),
                ));
            }
            Some("same_model" | "unknown_source") if !warnings.is_empty() => {
                body = body.push(banner(false, warnings, p));
            }
            _ => {}
        }
        if let Some(blocked) = diff["blocked"].as_str() {
            body = body.push(banner(true, blocked.to_owned(), p));
        } else if diff["restarts_stream"] == true {
            body = body.push(
                text(format!(
                    "Restoring stops and restarts the stream to change {}.",
                    and_list(&locked(diff))
                ))
                .size(style::SMALL)
                .color(p.secondary),
            );
        }
        let count = changes(diff).count();
        let mut section = column![heading_owned(format!("Changes ({count})"), p)].spacing(8);
        if count == 0 {
            section = section.push(
                text("Everything this setup can change already matches the camera.")
                    .size(style::BODY)
                    .color(p.secondary),
            );
        } else {
            if disruptive(diff) {
                section = section.push(
                    text("Settings marked “Changes camera behavior” control how the camera triggers or signals other equipment.")
                        .size(style::SMALL)
                        .color(p.secondary),
                );
            }
            let header = |label: &'static str, portion| {
                text(label)
                    .size(style::CAPTION)
                    .font(style::SEMIBOLD)
                    .color(p.secondary)
                    .width(Length::FillPortion(portion))
            };
            section = section
                .push(row![header("Feature", 5), header("Now", 3), header("Setup", 3)].spacing(10));
            for change in changes(diff) {
                section = section.push(change_row(change, p));
            }
        }
        body = body.push(section);
        let not_applied = diff["not_applied"].as_array().cloned().unwrap_or_default();
        if !not_applied.is_empty() {
            let mut section = column![heading_owned(
                format!("Not applied ({})", not_applied.len()),
                p
            )]
            .spacing(6);
            for row in &not_applied {
                let place = banks(row);
                section = section.push(
                    row![
                        text(row["feature"].as_str().unwrap_or_default().to_owned())
                            .size(style::BODY)
                            .color(p.text),
                        text(place).size(style::SMALL).color(p.secondary),
                        space::horizontal(),
                        text(capitalize(row["reason"].as_str().unwrap_or_default()))
                            .size(style::SMALL)
                            .color(p.secondary),
                    ]
                    .spacing(6)
                    .align_y(Alignment::Center),
                );
            }
            body = body.push(section);
        }
        let unchanged = diff["unchanged"].as_u64().unwrap_or_default();
        if unchanged > 0 {
            body = body.push(
                text(format!(
                    "{} already {}.",
                    plural(unchanged, "setting", "settings"),
                    if unchanged == 1 { "matches" } else { "match" }
                ))
                .size(style::SMALL)
                .color(p.secondary),
            );
        }
        body.into()
    }

    fn setup_restore_footer(&self, p: &'static Palette) -> Element<'_, Message> {
        let state = &self.setups;
        let name = state.chosen.clone().unwrap_or_default();
        if state.confirm_delete {
            let cancel = button(text("Cancel").size(style::BODY))
                .padding([7, 14])
                .style(style::secondary)
                .on_press(Message::Setups(Input::ConfirmDelete(false)));
            let delete = button(text("Delete").size(style::BODY))
                .name(format!("Delete setup {name}"))
                .padding([7, 14])
                .style(style::danger)
                .on_press_maybe((!state.deleting).then_some(Message::Setups(Input::Delete)));
            let (first, second) = dialog_actions(cancel, delete, false);
            return row![
                text(format!("Delete {name}? This removes only the saved setup."))
                    .size(style::SMALL)
                    .color(p.text)
                    .width(Fill),
                first,
                second
            ]
            .spacing(8)
            .align_y(Alignment::Center)
            .into();
        }
        let diff = state.diff();
        let count = diff.map_or(0, |diff| changes(diff).count());
        let applying = self.pending(Job::ApplySetup);
        let label = if applying {
            "Restoring…".to_owned()
        } else if state.confirm {
            "Confirm Restore".to_owned()
        } else if count == 0 {
            "Nothing to Change".to_owned()
        } else {
            format!("Restore {}", plural(count as u64, "Change", "Changes"))
        };
        let enabled = diff.is_some_and(|diff| self.can_restore_setup(diff));
        let restore = button(text(label.clone()).size(style::BODY))
            .name(label)
            .padding([7, 18])
            .style(if state.confirm {
                style::danger
            } else {
                style::primary
            })
            .on_press_maybe(enabled.then_some(Message::Setups(Input::Restore)));
        let cancel = button(text("Cancel").size(style::BODY))
            .padding([7, 14])
            .style(style::secondary)
            .on_press(Message::Setups(Input::Open(None)));
        let (first, second) = dialog_actions(cancel, restore, false);
        let footer = row![
            button(text("Delete Setup…").size(style::SMALL))
                .name(format!("Delete setup {name}…"))
                .padding([6, 8])
                .style(style::plain)
                .on_press_maybe(
                    (!applying && !state.deleting)
                        .then_some(Message::Setups(Input::ConfirmDelete(true)))
                ),
            space::horizontal(),
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        footer.push(first).push(second).into()
    }

    /// "Save setup…" and "Restore…" above the features, unless searching.
    pub(super) fn setup_links(&self, searching: bool, p: &'static Palette) -> Element<'_, Message> {
        if searching || self.snapshot.active_camera.is_none() {
            return space().into();
        }
        let link = |label: &'static str, name: &'static str, on: Option<Message>| {
            button(text(label).size(style::SMALL).font(style::MEDIUM))
                .name(name)
                .padding([3, 8])
                .style(style::link)
                .on_press_maybe(on)
        };
        row![
            text("Saved setups").size(style::SMALL).color(p.secondary),
            space::horizontal(),
            link(
                "Save setup…",
                "Save setup…",
                (!self.history_busy()).then_some(Message::Setups(Input::Open(Some(Page::Save)))),
            ),
            link(
                "Restore…",
                "Restore setup…",
                Some(Message::Setups(Input::Open(Some(Page::List)))),
            ),
        ]
        .align_y(Alignment::Center)
        .into()
    }

    /// Scenes: `setups` (two saved), `setup-save` (what is not saved, open),
    /// `setup-restore` (changes with a disruptive trigger mode, a value the
    /// camera lacks and a stream restart), `setup-mismatch` (another model,
    /// not yet allowed) and `setup-blocked` (a recording holds the stream).
    pub(super) fn scene_setups(&mut self, word: &str, late: bool) -> bool {
        if !matches!(
            word,
            "setups" | "setup-save" | "setup-restore" | "setup-mismatch" | "setup-blocked"
        ) {
            return false;
        }
        if !late {
            // The sheet compares with a connected camera.
            return false;
        }
        let camera = self.snapshot.active_camera.clone();
        self.setups = State {
            open: true,
            scene: true,
            camera: camera.clone(),
            dir: Some(crate::ipc::state_dir().join("setups")),
            list: Some(Ok(scene_list())),
            ..State::default()
        };
        let live = self.setup_camera_info().cloned();
        let camera_json = live.as_ref().map_or(Value::Null, |info| {
            serde_json::json!({"id":info.id,"vendor":info.vendor,"model":info.model,"serial":info.serial})
        });
        match word {
            "setups" => {}
            "setup-save" => {
                self.setups.page = Page::Save;
                self.setups.name = "line-2-mono".into();
                self.setups.note = "Ring light at 60%".into();
                self.setups.skipped_open = true;
                self.setups.reading = Some(Ok(scene_reading(live.as_ref())));
            }
            _ => {
                self.setups.page = Page::Restore;
                let mismatch = word == "setup-mismatch";
                let name = if mismatch {
                    "bench-basler"
                } else {
                    "line-2-mono"
                };
                self.setups.chosen = Some(name.into());
                self.setups.diff = Some(Ok(scene_diff(
                    name,
                    camera_json,
                    mismatch,
                    word == "setup-blocked",
                )));
            }
        }
        true
    }
}

fn scene_list() -> Vec<Summary> {
    vec![
        Summary {
            name: "bench-basler".into(),
            transport: crate::types::Transport::GigE,
            vendor: "Basler".into(),
            model: "acA1920-40gm".into(),
            serial: Some("23456789".into()),
            saved_at: "2026-09-14T09:12:00Z".into(),
            values: 42,
            note: Some("Tuned for the inspection bench".into()),
        },
        Summary {
            name: "line-2-mono".into(),
            transport: crate::types::Transport::Simulator,
            vendor: "Capturefab".into(),
            model: "Pattern camera".into(),
            serial: Some("SIM0".into()),
            saved_at: "2026-10-25T14:03:11Z".into(),
            values: 6,
            note: Some("Ring light at 60%".into()),
        },
    ]
}

fn scene_reading(live: Option<&CameraInfo>) -> Reading {
    let value = |feature: &str, value: Value| store::SetupValue {
        feature: feature.into(),
        value: store::Scalar::from_value(&value).expect("a scalar"),
        at: Vec::new(),
    };
    let skip = |feature: &str, at: Option<(&str, &str)>, reason: &str| store::Skipped {
        feature: feature.into(),
        at: at
            .map(|(selector, value)| {
                vec![store::Bank {
                    selector: selector.into(),
                    value: value.into(),
                }]
            })
            .unwrap_or_default(),
        reason: reason.into(),
    };
    Reading {
        camera: live.map_or_else(
            || store::SavedCamera {
                transport: crate::types::Transport::Simulator,
                vendor: "Capturefab".into(),
                model: "Pattern camera".into(),
                serial: Some("SIM0".into()),
                id: None,
                kind: Default::default(),
            },
            store::SavedCamera::from_info,
        ),
        auto: None,
        values: vec![
            value("PixelFormat", Value::from("Mono8")),
            value("Width", Value::from(640)),
            value("Height", Value::from(480)),
            value("AcquisitionFrameRate", Value::from(30.0)),
            value("Gain", Value::from(0.0)),
            value("BlackLevel", Value::from(4.0)),
        ],
        skipped: vec![
            skip("ExposureTime", None, "set by ExposureAuto = Continuous"),
            skip(
                "LineSource",
                Some(("LineSelector", "Line3")),
                "could not read at LineSelector = Line3: timed out",
            ),
            skip(
                "LineSource",
                Some(("LineSelector", "Line0")),
                "read-only at LineSelector = Line0",
            ),
        ],
    }
}

fn scene_diff(name: &str, camera: Value, mismatch: bool, blocked: bool) -> Value {
    let mut changes = vec![
        serde_json::json!({"feature":"Width","saved":640,"current":320,"risk":"reversible","stream_locked":true}),
        serde_json::json!({"feature":"ExposureTime","saved":10000.0,"current":5000.0,"risk":"reversible","stream_locked":false}),
        serde_json::json!({"feature":"Gain","at":[{"selector":"GainSelector","value":"All"}],"saved":0.0,"current":3.0,"risk":"reversible","stream_locked":false}),
    ];
    if !mismatch && !blocked {
        changes.push(serde_json::json!({"feature":"TriggerMode","at":[{"selector":"TriggerSelector","value":"FrameStart"}],"saved":"On","current":"Off","risk":"disruptive","stream_locked":false}));
    }
    let not_applied = if mismatch {
        vec![
            serde_json::json!({"feature":"LineSource","at":[{"selector":"LineSelector","value":"Line2"}],"status":"missing","saved":"ExposureActive","reason":"LineSelector is not on this camera"}),
            serde_json::json!({"feature":"Scan3dOutputMode","status":"missing","saved":"Calibrated","reason":"not on this camera"}),
        ]
    } else {
        vec![
            serde_json::json!({"feature":"Scan3dOutputMode","status":"missing","saved":"Calibrated","reason":"not on this camera"}),
        ]
    };
    let disruptive = changes.iter().any(|change| change["risk"] == "disruptive");
    serde_json::json!({
        "setup": name,
        "digest": format!("sha256:{}", "0".repeat(64)),
        "camera": camera,
        "match": if mismatch { "other_model" } else { "same_camera" },
        "warnings": [],
        "changes": changes,
        "unchanged": if mismatch { 9 } else { 30 },
        "not_applied": not_applied,
        "risk": if disruptive { "disruptive" } else { "reversible" },
        "restarts_stream": true,
        "blocked": blocked.then_some("Stop recording and scheduled captures first: this setup changes Width."),
    })
}

/// A section title that owns its text.
fn heading_owned<'a>(title: String, p: &'static Palette) -> Element<'a, Message> {
    let content = container(
        text(title.clone())
            .size(style::CAPTION)
            .font(style::SEMIBOLD)
            .color(p.secondary),
    )
    .padding(iced::Padding {
        top: 6.0,
        ..iced::Padding::ZERO
    })
    .into();
    accessibility::label(content, title, accessibility::Role::Heading)
}

/// A note across the sheet: a warning, or information.
fn banner<'a>(warning: bool, message: String, p: &'static Palette) -> Element<'a, Message> {
    let mark = if warning {
        Level::Warning.mark(13.0, p)
    } else {
        icon(Icon::Camera, 13.0, p.secondary)
    };
    let content = container(
        row![
            container(mark)
                .height(Length::Fixed(style::line_height(style::BODY)))
                .align_y(Alignment::Center),
            text(message.clone()).size(style::BODY).color(p.text),
        ]
        .spacing(8),
    )
    .padding([8, 10])
    .width(Fill)
    .style(style::well)
    .into();
    accessibility::label(content, message, accessibility::Role::Status)
}

fn change_row<'a>(change: &Value, p: &'static Palette) -> Element<'a, Message> {
    let feature = change["feature"].as_str().unwrap_or_default().to_owned();
    let mut name = row![text(feature).size(style::BODY).color(p.text)]
        .spacing(5)
        .align_y(Alignment::Center);
    let place = banks(change);
    if !place.is_empty() {
        name = name.push(text(place).size(style::SMALL).color(p.secondary));
    }
    let mut cell = column![name].spacing(3);
    if change["risk"] == "disruptive" {
        cell = cell.push(
            container(
                text("Changes camera behavior")
                    .size(style::CAPTION)
                    .font(style::MEDIUM)
                    .color(p.ink(p.warn)),
            )
            .padding([1, 6])
            .style(style::pill(p.warn)),
        );
    }
    if let Some(warning) = change["warning"].as_str() {
        cell = cell.push(
            text(capitalize(warning))
                .size(style::CAPTION)
                .color(p.secondary),
        );
    }
    row![
        cell.width(Length::FillPortion(5)),
        text(shown(&change["current"]))
            .size(style::BODY)
            .color(p.secondary)
            .width(Length::FillPortion(3)),
        text(shown(&change["saved"]))
            .size(style::BODY)
            .font(style::MEDIUM)
            .color(p.text)
            .width(Length::FillPortion(3)),
    ]
    .spacing(10)
    .align_y(Alignment::Start)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced::advanced::{Layout, layout, widget::Tree};
    use iced::{Event, keyboard, mouse};

    fn bench(word: &str) -> Workbench {
        let mut bench = Workbench::new(SessionHandle::new(), "setups".into(), true, None);
        let camera = liveness::streaming_camera(0, 0, 0.0);
        bench.snapshot.connected = Some(camera.info.clone());
        bench.snapshot.active_camera = Some(camera.info.id.clone());
        bench.snapshot.cameras = vec![camera];
        bench.width = 1200.0;
        assert!(!bench.scene_setups(word, false), "waits for the cameras");
        assert!(bench.scene_setups(word, true));
        bench
    }

    fn renderer() -> iced::Renderer {
        super::super::destinations::layout_tests::renderer()
    }

    /// The sheet's buttons by name: whether each can be pressed.
    fn buttons(bench: &Workbench) -> Vec<(String, bool)> {
        let renderer = renderer();
        let mut view = bench.setups_sheet(Palette::of(false), 800.0);
        let mut tree = Tree::new(view.as_widget());
        let node = view.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(1200.0, 800.0)),
        );
        accessibility::snapshot_widget(
            view.as_widget_mut(),
            &mut tree,
            Layout::new(&node),
            &renderer,
        )
        .0
        .nodes
        .into_iter()
        .filter(|(_, node)| node.role() == accessibility::Role::Button)
        .filter_map(|(_, node)| Some((node.label()?.to_owned(), !node.is_disabled())))
        .collect()
    }

    fn enabled(bench: &Workbench, label: &str) -> bool {
        buttons(bench)
            .into_iter()
            .find(|(name, _)| name == label)
            .unwrap_or_else(|| panic!("no {label} in {:?}", buttons(bench)))
            .1
    }

    #[test]
    fn another_model_needs_the_box_ticked_before_restoring() {
        let mut bench = bench("setup-mismatch");
        assert!(!enabled(&bench, "Restore 3 Changes"));
        let _ = bench.setups_input(Input::AllowOtherModel(true));
        assert!(enabled(&bench, "Restore 3 Changes"));
        let _ = bench.setups_input(Input::AllowOtherModel(false));
        assert!(!enabled(&bench, "Restore 3 Changes"));
    }

    #[test]
    fn a_blocked_restore_cannot_be_pressed() {
        let bench = bench("setup-blocked");
        assert!(!enabled(&bench, "Restore 3 Changes"));
        assert!(enabled(&bench, "Cancel"));
    }

    #[test]
    fn disruptive_changes_need_a_second_click_and_return_never_restores() {
        let mut bench = bench("setup-restore");
        assert!(enabled(&bench, "Restore 4 Changes"));
        let _ = bench.setups_input(Input::Restore);
        assert!(bench.setups.confirm);
        assert!(enabled(&bench, "Confirm Restore"));
        // Changing the decision asks again.
        let _ = bench.setups_input(Input::AllowOtherModel(false));
        assert!(!bench.setups.confirm);

        let renderer = renderer();
        let mut view = bench.setups_sheet(Palette::of(false), 800.0);
        let mut tree = Tree::new(view.as_widget());
        let node = view.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(1200.0, 800.0)),
        );
        let key = keyboard::Key::Named(keyboard::key::Named::Enter);
        let event = Event::Keyboard(keyboard::Event::KeyPressed {
            key: key.clone(),
            modified_key: key,
            physical_key: keyboard::key::Physical::Unidentified(
                keyboard::key::NativeCode::Unidentified,
            ),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: None,
            repeat: false,
        });
        let mut messages = Vec::new();
        let mut shell = iced::advanced::Shell::new(&mut messages);
        view.as_widget_mut().update(
            &mut tree,
            &event,
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &renderer,
            &mut iced::advanced::clipboard::Null,
            &mut shell,
            &iced::Rectangle::with_size(Size::new(1200.0, 800.0)),
        );
        assert!(
            !messages
                .iter()
                .any(|message| matches!(message, Message::Setups(Input::Restore))),
            "Return restored a setup"
        );
    }

    #[test]
    fn the_sheet_blocks_menus_and_shows_what_is_not_saved() {
        let mut bench = bench("setup-save");
        assert!(bench.sheet_open());
        let items = bench
            .command_menus()
            .into_iter()
            .flat_map(|menu| menu.items)
            .flatten()
            .filter(|item| {
                matches!(
                    item.command,
                    menus::Command::SaveSetup | menus::Command::RestoreSetup
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(items.len(), 2);
        assert!(items.iter().all(|item| !item.enabled));
        assert!(enabled(&bench, "Save setup"));
        // Escape closes it.
        let _ = bench.dispatch_action(Action::Overview);
        assert!(!bench.sheet_open());
    }

    #[test]
    fn invalid_names_are_explained_in_place() {
        let mut bench = bench("setup-save");
        let _ = bench.setups_input(Input::Name("con".into()));
        let _ = bench.setups_input(Input::Save(false));
        assert!(
            bench
                .setups
                .name_error
                .as_deref()
                .is_some_and(|error| error.starts_with("Setup names are"))
        );
        let _ = bench.setups_input(Input::Name("ok".into()));
        assert!(bench.setups.name_error.is_none());
    }

    #[test]
    fn scene_words_name_only_setup_states() {
        let mut bench = Workbench::new(SessionHandle::new(), "setups".into(), true, None);
        for word in ["setups-x", "setup", "resources"] {
            assert!(!bench.scene_setups(word, true), "{word}");
        }
        for (word, page) in [
            ("setups", Page::List),
            ("setup-save", Page::Save),
            ("setup-restore", Page::Restore),
            ("setup-mismatch", Page::Restore),
            ("setup-blocked", Page::Restore),
        ] {
            assert!(bench.scene_setups(word, true));
            assert_eq!(bench.setups.page, page, "{word}");
        }
        assert_eq!(
            and_list(&["Width", "Height", "PixelFormat"]),
            "Width, Height and PixelFormat"
        );
        assert_eq!(saved_on("not a date"), "not a date");
    }
}
