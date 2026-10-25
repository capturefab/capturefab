//! "Check for Updates…": the Software Update sheet, Settings › Updates and
//! the daily automatic check. Fetches run on their own thread and their
//! result arrives on `tick()`; nothing is downloaded or installed.
use super::*;
use crate::updates::{self as check, Outcome, Policy, PolicySource, Settings, Status, UpdateError};
use iced::widget::column;
use std::sync::mpsc;

/// The first automatic check waits this long after launch.
const LAUNCH_DELAY: Duration = Duration::from_secs(30);
/// How often an open app asks whether a daily check is due.
const RECHECK: Duration = Duration::from_secs(60 * 60);
/// Opening the sheet again within this long shows the last result.
const FRESH: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Input {
    Open,
    Close,
    /// From the guide's update row: close it, then open the sheet.
    FromHelp,
    CheckNow,
    Download,
    ReleaseNotes,
    OpenDownloads,
    Skip,
    SetAutomatic(bool),
    Scrolled(bool),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Run {
    Requested,
    Automatic,
}

struct Fetched {
    /// None when an automatic check was not due after all.
    result: Option<Result<Outcome, UpdateError>>,
    settings: Settings,
}

pub(super) struct State {
    pub(super) open: bool,
    running: Option<(Run, mpsc::Receiver<Fetched>)>,
    pub(super) settings: Settings,
    pub(super) policy: Policy,
    /// When the last check of this session finished.
    checked: Option<Instant>,
    announced: Option<String>,
    pub(super) announce_pending: Option<String>,
    next_due: Option<Instant>,
    /// A screenshot scene: never fetch.
    scene: bool,
    scene_sender: Option<mpsc::Sender<Fetched>>,
    scrolled: bool,
    /// Fetches started, for tests.
    pub(super) started: usize,
}

impl Default for State {
    fn default() -> Self {
        Self {
            open: false,
            running: None,
            settings: Settings::default(),
            policy: Policy::Allowed,
            checked: None,
            announced: None,
            announce_pending: None,
            next_due: None,
            scene: false,
            scene_sender: None,
            scrolled: false,
            started: 0,
        }
    }
}

impl State {
    /// What the last check found, read once at launch. Tests start empty.
    pub(super) fn load() -> Self {
        if cfg!(test) {
            return Self::default();
        }
        let settings = Settings::load(&crate::ipc::state_dir());
        Self {
            announce_pending: settings.announce(),
            policy: check::policy(),
            settings,
            ..Self::default()
        }
    }

    pub(super) fn checking(&self) -> bool {
        self.running.is_some()
    }

    /// The version the menu, guide and notice announce.
    pub(super) fn announce(&self) -> Option<String> {
        self.settings.announce()
    }
}

#[cfg(test)]
struct Offline;
#[cfg(test)]
impl check::Fetcher for Offline {
    fn get(&self, _: &check::Url) -> Result<Vec<u8>, UpdateError> {
        Err(UpdateError::Offline)
    }
}

fn fetcher() -> Result<Box<dyn check::Fetcher + Send + Sync>, UpdateError> {
    // Tests never reach a network.
    #[cfg(test)]
    {
        Ok(Box::new(Offline))
    }
    #[cfg(not(test))]
    {
        check::native()
    }
}

fn spawn(job: Run) -> Option<mpsc::Receiver<Fetched>> {
    let (send, receive) = mpsc::channel();
    let dir = crate::ipc::state_dir();
    std::thread::Builder::new()
        .name("capturefab-updates".into())
        .spawn(move || {
            let now = chrono::Utc::now();
            let fetched = match fetcher() {
                Err(error) => Fetched {
                    result: Some(Err(error)),
                    settings: Settings::load(&dir),
                },
                Ok(fetcher) => match job {
                    Run::Requested => match check::check_with(
                        fetcher.as_ref(),
                        &dir,
                        check::manifest_url(),
                        &check::policy(),
                        now,
                    ) {
                        Ok((outcome, settings)) => Fetched {
                            result: Some(Ok(outcome)),
                            settings,
                        },
                        Err(error) => Fetched {
                            result: Some(Err(error)),
                            settings: Settings::load(&dir),
                        },
                    },
                    Run::Automatic => match check::automatic_with(fetcher.as_ref(), &dir, now) {
                        Some((result, settings)) => Fetched {
                            result: Some(result),
                            settings,
                        },
                        None => Fetched {
                            result: None,
                            settings: Settings::load(&dir),
                        },
                    },
                },
            };
            let _ = send.send(fetched);
        })
        .ok()?;
    Some(receive)
}

/// "today at 09:14", "yesterday at 18:02" or "on 20 Nov 2026", local time.
fn when(settings: &Settings) -> Option<String> {
    let at = settings.last_checked_at()?.with_timezone(&chrono::Local);
    let today = chrono::Local::now().date_naive();
    Some(if at.date_naive() == today {
        format!("today at {}", at.format("%H:%M"))
    } else if today.pred_opt() == Some(at.date_naive()) {
        format!("yesterday at {}", at.format("%H:%M"))
    } else {
        format!("on {}", at.format("%-d %b %Y"))
    })
}

fn release_date(date: &str) -> Option<String> {
    chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .ok()
        .map(|date| date.format("%-d %b %Y").to_string())
}

fn policy_text(source: &PolicySource) -> String {
    format!(
        "Update checks are turned off on this computer by {}.",
        source.describe()
    )
}

impl Workbench {
    fn start_update_check(&mut self, job: Run) {
        if self.updates.running.is_some() || self.updates.scene || !self.updates.policy.allowed() {
            return;
        }
        if let Some(receive) = spawn(job) {
            self.updates.running = Some((job, receive));
            self.updates.started += 1;
        }
    }

    fn save_update_settings(&mut self, change: impl FnOnce(&mut Settings)) {
        if self.updates.scene {
            change(&mut self.updates.settings);
            return;
        }
        match Settings::update(&crate::ipc::state_dir(), change) {
            Ok(settings) => self.updates.settings = settings,
            Err(error) => self.set_notice(
                "Couldn't save the update setting",
                Some(error.to_string()),
                Level::Error,
            ),
        }
    }

    fn open_update_link(&mut self, url: Option<String>) {
        let Some(url) = url else { return };
        if let Err(error) = reveal::open_link(&url) {
            self.set_notice(
                "Couldn't open the link",
                Some(format!("{url}: {error}")),
                Level::Error,
            );
        }
    }

    pub(super) fn updates_input(&mut self, input: Input) -> Task<Message> {
        match input {
            Input::Open => {
                if self.close_error.is_some() || self.settings_window == Some(self.focused_window) {
                    return Task::none();
                }
                if !self.updates.open {
                    if self.sheet_open() {
                        return Task::none();
                    }
                    self.updates.open = true;
                    self.updates.scrolled = false;
                    let fresh = self
                        .updates
                        .checked
                        .is_some_and(|at| self.now.saturating_duration_since(at) < FRESH);
                    if !fresh {
                        self.start_update_check(Run::Requested);
                    }
                }
                return operation::focus("sheet-close");
            }
            Input::Close => self.updates.open = false,
            Input::FromHelp => {
                self.show_help(false);
                return self.updates_input(Input::Open);
            }
            Input::CheckNow => self.start_update_check(Run::Requested),
            Input::Download => {
                let url = self
                    .updates
                    .settings
                    .last_outcome
                    .as_ref()
                    .and_then(|outcome| outcome.download.as_ref())
                    .map(|download| download.url.clone());
                self.open_update_link(url);
            }
            Input::ReleaseNotes => {
                let url = self
                    .updates
                    .settings
                    .last_outcome
                    .as_ref()
                    .and_then(|outcome| outcome.url.clone());
                self.open_update_link(url);
            }
            Input::OpenDownloads => self.open_update_link(Some(check::DOWNLOADS_URL.into())),
            Input::Skip => {
                if let Status::Available(version) = self.updates.settings.status() {
                    if self.updates.announce_pending.as_deref() == Some(&version) {
                        self.updates.announce_pending = None;
                    }
                    self.save_update_settings(|settings| settings.skipped = Some(version));
                }
                self.updates.open = false;
            }
            Input::SetAutomatic(on) => {
                if self.updates.policy.allowed() {
                    self.save_update_settings(|settings| settings.automatic = Some(on));
                    if on {
                        // Ask soon whether a check is due.
                        self.updates.next_due = Some(self.now);
                    }
                }
            }
            Input::Scrolled(scrolled) => self.updates.scrolled = scrolled,
        }
        Task::none()
    }

    /// Collect a finished check, start a due automatic one and post an
    /// announcement when nothing else is going on; from `tick()`.
    pub(super) fn tick_updates(&mut self) {
        if let Some((job, receive)) = &self.updates.running {
            let job = *job;
            match receive.try_recv() {
                Ok(fetched) => {
                    self.updates.running = None;
                    self.updates.settings = fetched.settings;
                    if fetched.result.is_some() {
                        self.updates.checked = Some(self.now);
                    }
                    if job == Run::Automatic
                        && let Some(version) = self.updates.announce()
                        && self.updates.announced.as_deref() != Some(&version)
                    {
                        self.updates.announce_pending = Some(version);
                    }
                }
                Err(mpsc::TryRecvError::Empty) => (),
                Err(mpsc::TryRecvError::Disconnected) => self.updates.running = None,
            }
        }
        // Screenshot mode is set after construction, so it is checked here.
        if !cfg!(test)
            && self.screenshot.is_none()
            && !self.updates.scene
            && self.updates.running.is_none()
            && self.updates.policy.allowed()
            && self.updates.settings.automatic()
        {
            let due = *self
                .updates
                .next_due
                .get_or_insert(self.born + LAUNCH_DELAY);
            if self.now >= due {
                self.updates.next_due = Some(self.now + RECHECK);
                self.start_update_check(Run::Automatic);
            }
        }
        self.announce_update();
    }

    /// The one notice per version and launch, only into an empty slot and
    /// never while work is pending, a sheet is open or anything records.
    fn announce_update(&mut self) {
        let Some(version) = self.updates.announce_pending.clone() else {
            return;
        };
        if self.updates.announce().as_deref() != Some(&version)
            || self.updates.announced.as_deref() == Some(&version)
        {
            self.updates.announce_pending = None;
            return;
        }
        let recording = self.snapshot.forwarding.is_some()
            || self
                .snapshot
                .cameras
                .iter()
                .any(|camera| camera.forwarding.is_some());
        if self.notice.is_some()
            || !self.pending.is_empty()
            || self.sheet_open()
            || recording
            || self.updates.scene
        {
            return;
        }
        self.set_notice(
            format!("Capturefab {version} is available. Open Check for Updates for details."),
            None,
            Level::Done,
        );
        self.updates.announced = Some(version);
        self.updates.announce_pending = None;
    }

    /// The guide's one-line pointer to an available version.
    pub(super) fn updates_help_row(&self, p: &'static Palette) -> Option<Element<'_, Message>> {
        let version = self.updates.announce()?;
        Some(
            container(
                row![
                    icon(Icon::CloudArrowUp, 16.0, p.accent_text),
                    text(format!("Capturefab {version} is available."))
                        .size(style::BODY)
                        .width(Fill),
                    button(text("View").size(style::BODY))
                        .name("View the available Capturefab version")
                        .style(style::secondary)
                        .padding([5, 10])
                        .on_press(Message::Updates(Input::FromHelp)),
                ]
                .spacing(10)
                .align_y(Alignment::Center),
            )
            .padding([8, 12])
            .width(Fill)
            .style(style::well)
            .into(),
        )
    }

    /// Settings › Updates.
    pub(super) fn updates_settings_section(&self, p: &'static Palette) -> Element<'_, Message> {
        let state = &self.updates;
        let allowed = state.policy.allowed();
        let small = |value: String| {
            text(value)
                .size(style::SMALL)
                .color(p.secondary)
                .wrapping(text::Wrapping::WordOrGlyph)
        };
        let mut section = column![
            heading("Updates", p),
            checkbox_maybe(
                "Check for new versions automatically",
                allowed && state.settings.automatic(),
                allowed.then_some(|on| Message::Updates(Input::SetAutomatic(on))),
            ),
            small(
                "Once a day, Capturefab downloads capturefab.com/releases.json, the public list of releases. It sends no account, device or usage information and sets no cookies; like any website visit, the server sees your IP address. Capturefab never downloads or installs updates for you."
                    .into()
            ),
        ]
        .spacing(9);
        let status: Element<'_, Message> = match &state.policy {
            Policy::Off(source) => small(policy_text(source)).into(),
            Policy::Allowed if state.checking() => text("Checking…")
                .size(style::SMALL)
                .color(p.accent_text)
                .into(),
            Policy::Allowed => match (state.settings.status(), when(&state.settings)) {
                (Status::Available(version), _) => row![
                    text(format!("{version} is available."))
                        .size(style::SMALL)
                        .font(style::MEDIUM),
                    button(text("Release Notes").size(style::SMALL))
                        .name("Open the release notes")
                        .style(style::link)
                        .padding(0)
                        .on_press_maybe(
                            state
                                .settings
                                .last_outcome
                                .as_ref()
                                .and_then(|outcome| outcome.url.as_ref())
                                .map(|_| Message::Updates(Input::ReleaseNotes))
                        ),
                ]
                .spacing(8)
                .align_y(Alignment::Center)
                .into(),
                (_, None) => small("Never checked.".into()).into(),
                (Status::Failed(error), _) => small(format!("Last check failed: {error}.")).into(),
                (Status::Newer(latest), Some(at)) => small(format!(
                    "Last checked {at}. This build ({}) is newer than the latest release, {latest}.",
                    check::current()
                ))
                .into(),
                (Status::None, Some(at)) => {
                    small(format!("Last checked {at}. No releases are published yet.")).into()
                }
                (_, Some(at)) => small(format!(
                    "Last checked {at}. {} is the latest version.",
                    check::current()
                ))
                .into(),
            },
        };
        section = section.push(
            row![
                container(status).width(Fill),
                button(text("Check Now").size(style::SMALL))
                    .name("Check for a new version now")
                    .style(style::secondary)
                    .padding([6, 10])
                    .on_press_maybe(
                        (allowed && !state.checking()).then_some(Message::Updates(Input::CheckNow))
                    ),
            ]
            .spacing(12)
            .align_y(Alignment::Center),
        );
        section.into()
    }

    /// The Software Update sheet.
    pub(super) fn updates_sheet(&self, p: &'static Palette, height: f32) -> Element<'_, Message> {
        let state = &self.updates;
        let current = check::current();
        let title = |value: String| {
            text(value)
                .size(style::TITLE)
                .font(style::SEMIBOLD)
                .wrapping(text::Wrapping::WordOrGlyph)
        };
        let body_text = |value: String| {
            text(value)
                .size(style::BODY)
                .color(p.secondary)
                .wrapping(text::Wrapping::WordOrGlyph)
        };
        let action = |label: String, name: &'static str, message: Input, primary: bool| {
            button(text(label).size(style::BODY))
                .name(name)
                .style(if primary {
                    style::primary
                } else {
                    style::secondary
                })
                .padding([7, 12])
                .on_press(Message::Updates(message))
        };
        let downloads = || {
            action(
                "Open Downloads Page".into(),
                "Open the Capturefab downloads page",
                Input::OpenDownloads,
                false,
            )
        };
        let mut body = column![].spacing(14);
        let mut footer: Option<Element<'_, Message>> = None;
        let outcome = state.settings.last_outcome.clone().unwrap_or_default();
        match (&state.policy, state.checking(), state.settings.status()) {
            (Policy::Off(source), _, _) => {
                body = body
                    .push(title("Update checks are off".into()))
                    .push(body_text(format!(
                        "{} You're using {current}.",
                        policy_text(source)
                    )))
                    .push(downloads());
            }
            (_, true, _) => {
                body = body.push(
                    row![
                        icon::spinner(18.0, p.accent_text, self.spin()),
                        body_text("Checking capturefab.com for a newer version…".into()),
                    ]
                    .spacing(10)
                    .align_y(Alignment::Center),
                );
                footer = Some(
                    row![
                        space().width(Fill),
                        action(
                            "Cancel".into(),
                            "Stop waiting for the check",
                            Input::Close,
                            false
                        )
                    ]
                    .into(),
                );
            }
            (_, false, Status::Available(version)) => {
                let mut subtitle = format!("You have {current}.");
                if let Some(date) = outcome.date.as_deref().and_then(release_date) {
                    subtitle.push_str(&format!(" Released {date}."));
                }
                body = body
                    .push(title(format!("Capturefab {version} is available")))
                    .push(body_text(subtitle));
                if !outcome.highlights.is_empty() {
                    body = body.push(
                        column(outcome.highlights.iter().map(|line| {
                            row![
                                text("•").size(style::BODY).color(p.secondary),
                                text(line.clone())
                                    .size(style::BODY)
                                    .wrapping(text::Wrapping::WordOrGlyph)
                                    .width(Fill),
                            ]
                            .spacing(8)
                            .into()
                        }))
                        .spacing(6),
                    );
                }
                if state.settings.skipped.as_deref() == Some(&version) {
                    body = body.push(body_text("You chose to skip this version.".into()));
                }
                let mut buttons = row![].spacing(10);
                match &outcome.download {
                    Some(download) => {
                        buttons = buttons.push(action(
                            format!("Download for {}", download.platform),
                            "Download the new version in your browser",
                            Input::Download,
                            true,
                        ));
                    }
                    None => {
                        body = body.push(body_text(format!(
                            "{version} has no download for {}.",
                            outcome
                                .platform
                                .clone()
                                .unwrap_or_else(check::host_platform)
                        )));
                        buttons = buttons.push(downloads());
                    }
                }
                if outcome.url.is_some() {
                    buttons = buttons.push(action(
                        "Release Notes".into(),
                        "Open the release notes",
                        Input::ReleaseNotes,
                        false,
                    ));
                }
                body = body.push(buttons);
                if let Some(download) = &outcome.download {
                    let source = if download.url.starts_with("https://github.com/") {
                        "Downloads come from GitHub."
                    } else {
                        "Downloads come from capturefab.com."
                    };
                    body = body
                        .push(
                            text(format!(
                                "{source} Check the file against SHA256SUMS; its SHA-256 is:"
                            ))
                            .size(style::SMALL)
                            .color(p.secondary)
                            .wrapping(text::Wrapping::WordOrGlyph),
                        )
                        .push(self.copyable(download.sha256.clone(), p))
                        .push(
                            text("Install it the same way you installed this version. Recordings and schedules keep running until you quit.")
                                .size(style::SMALL)
                                .color(p.secondary)
                                .wrapping(text::Wrapping::WordOrGlyph),
                        );
                }
                footer = Some(
                    row![
                        action(
                            "Skip This Version".into(),
                            "Don't announce this version again",
                            Input::Skip,
                            false
                        ),
                        space().width(Fill),
                        action("Later".into(), "Close until later", Input::Close, false),
                    ]
                    .spacing(10)
                    .into(),
                );
            }
            (_, false, Status::Failed(error)) => {
                body = body
                    .push(title("Couldn't check for updates".into()))
                    .push(body_text(format!("{error}.")))
                    .push(
                        row![
                            action("Try Again".into(), "Check again", Input::CheckNow, true),
                            downloads(),
                        ]
                        .spacing(10),
                    );
            }
            (_, false, Status::Unknown) => {
                body = body
                    .push(title("Not checked yet".into()))
                    .push(body_text(format!("You're using {current}.")))
                    .push(
                        row![
                            action(
                                "Check Now".into(),
                                "Check for a new version now",
                                Input::CheckNow,
                                true
                            ),
                            downloads(),
                        ]
                        .spacing(10),
                    );
            }
            (_, false, status) => {
                let checked = when(&state.settings)
                    .map(|at| format!(" Checked {at}."))
                    .unwrap_or_default();
                let (headline, detail) = match status {
                    Status::Newer(latest) => (
                        format!("Capturefab {current} is up to date"),
                        format!(
                            "This build ({current}) is newer than the latest release, {latest}.{checked}"
                        ),
                    ),
                    Status::None => (
                        format!("Capturefab {current} is up to date"),
                        format!("No releases are published yet.{checked}"),
                    ),
                    _ => (
                        format!("Capturefab {current} is up to date"),
                        format!("It is the latest version.{checked}"),
                    ),
                };
                body = body.push(title(headline)).push(body_text(detail));
                footer = Some(
                    row![
                        space().width(Fill),
                        action("Done".into(), "Close", Input::Close, true)
                    ]
                    .into(),
                );
            }
        }
        let mut sheet = sheet_frame("Software Update", Message::Updates(Input::Close), body, p)
            .mark(Icon::CloudArrowUp)
            .size(520.0f32.min((self.width - 48.0).max(0.0)), height)
            .scrolled(state.scrolled)
            .on_scroll(|value| Message::Updates(Input::Scrolled(value)));
        if let Some(footer) = footer {
            sheet = sheet.footer(footer);
        }
        sheet.into()
    }

    /// Screenshot words, which never fetch: `updates` (9.9.0 available),
    /// `updates-skipped`, `updates-current`, `updates-failed`,
    /// `updates-checking`, `updates-managed` and `updates-none`. With
    /// `settings` first, Settings shows instead of the sheet.
    pub(super) fn scene_updates(&mut self, word: &str, late: bool) -> bool {
        if !word.starts_with("updates") {
            return false;
        }
        let known = [
            "updates",
            "updates-skipped",
            "updates-current",
            "updates-failed",
            "updates-checking",
            "updates-managed",
            "updates-none",
        ];
        if !known.contains(&word) {
            return false;
        }
        if late {
            return true;
        }
        let state = &mut self.updates;
        state.scene = true;
        let now = chrono::Utc::now();
        state.settings = Settings {
            last_checked: Some(now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
            ..Settings::default()
        };
        let platform = check::host_platform();
        let available = Outcome {
            latest: Some("9.9.0".into()),
            date: Some(now.format("%Y-%m-%d").to_string()),
            url: Some("https://github.com/capturefab/capturefab/releases/tag/v9.9.0".into()),
            checksums_url: None,
            highlights: vec![
                "Faster previews for high-resolution GigE Vision cameras".into(),
                "Recordings can stop at a chosen size".into(),
                "Clearer messages when a camera is busy".into(),
            ],
            download: Some(check::Download {
                name: "capturefab-9.9.0.dmg".into(),
                url: "https://github.com/capturefab/capturefab/releases/download/v9.9.0/capturefab-9.9.0.dmg".into(),
                sha256: "ab12c3d4e5f60718293a4b5c6d7e8f90ab12c3d4e5f60718293a4b5c6d7eef90".into(),
                size: 52_000_000,
                platform: platform.clone(),
            }),
            platform: Some(platform),
            error: None,
            status: None,
        };
        match word {
            "updates" => state.settings.last_outcome = Some(available),
            "updates-skipped" => {
                state.settings.last_outcome = Some(available);
                state.settings.skipped = Some("9.9.0".into());
            }
            "updates-current" => {
                state.settings.last_outcome = Some(Outcome {
                    latest: Some(check::current().into()),
                    ..Outcome::default()
                });
            }
            "updates-failed" => {
                state.settings.last_outcome = Some(Outcome {
                    error: Some(UpdateError::Timeout.code().into()),
                    ..Outcome::default()
                });
            }
            "updates-none" => state.settings.last_outcome = Some(Outcome::default()),
            "updates-checking" => {
                let (send, receive) = mpsc::channel();
                state.scene_sender = Some(send);
                state.running = Some((Run::Requested, receive));
            }
            _ => {
                state.policy = Policy::Off(if cfg!(target_os = "macos") {
                    PolicySource::File(check::policy_paths().remove(0))
                } else {
                    PolicySource::Env
                });
            }
        }
        if !self.settings_open && !self.help_open {
            self.updates.open = true;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bench() -> Workbench {
        Workbench::new(SessionHandle::new(), "updates-test".into(), false, None)
    }

    fn found(app: &mut Workbench, version: &str) {
        app.updates.settings.last_outcome = Some(Outcome {
            latest: Some(version.into()),
            ..Outcome::default()
        });
        app.updates.settings.last_checked = Some("2026-10-25T09:14:00Z".into());
    }

    #[test]
    fn the_workbench_never_checks_on_its_own_in_tests_or_screenshots() {
        let mut app = bench();
        assert_eq!(app.updates.started, 0);
        app.now = app.born + Duration::from_secs(3600);
        app.tick_updates();
        assert_eq!(app.updates.started, 0);
        app.screenshot = None;
        assert!(!app.updates.checking());
    }

    #[test]
    fn opening_checks_once_and_refuses_to_stack_on_other_sheets() {
        let mut app = bench();
        app.help_open = true;
        let _ = app.updates_input(Input::Open);
        assert!(!app.updates.open, "never over the guide");
        app.help_open = false;
        let settings = window::Id::unique();
        app.settings_window = Some(settings);
        app.focused_window = settings;
        let _ = app.updates_input(Input::Open);
        assert!(!app.updates.open, "never behind a focused Settings window");
        app.focused_window = app.main_window;
        let _ = app.updates_input(Input::Open);
        assert!(app.updates.open);
        assert!(app.sheet_open());
        assert_eq!(app.updates.started, 1);
        // Opening again refocuses; it never starts a second fetch.
        let _ = app.updates_input(Input::Open);
        assert_eq!(app.updates.started, 1);
        // The stubbed fetch fails offline and is collected on tick.
        for _ in 0..200 {
            app.tick_updates();
            if !app.updates.checking() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!app.updates.checking());
    }

    #[test]
    fn settings_check_now_stays_in_settings_and_policy_disables_it() {
        let mut app = bench();
        app.settings_open = true;
        let _ = app.updates_input(Input::CheckNow);
        assert!(!app.updates.open);
        assert_eq!(app.updates.started, 1);
        let mut app = bench();
        app.updates.policy = Policy::Off(PolicySource::Env);
        let _ = app.updates_input(Input::CheckNow);
        let _ = app.updates_input(Input::SetAutomatic(false));
        assert_eq!(app.updates.started, 0);
        assert_eq!(app.updates.settings.automatic, None);
    }

    #[test]
    fn notices_wait_for_a_free_slot_and_skips_silence_one_version() {
        let mut app = bench();
        app.updates.scene = false;
        found(&mut app, "9.9.0");
        app.updates.announce_pending = Some("9.9.0".into());
        app.set_notice("Saved shot.png", None, Level::Done);
        app.tick_updates();
        assert_eq!(app.notice.as_ref().unwrap().text, "Saved shot.png");
        app.notice = None;
        app.send_batch(vec!["sim:0".into()], Job::Capture, |_| SessionCommand::Stop);
        app.tick_updates();
        assert!(app.notice.is_none(), "not while work is pending");
        app.pending.clear();
        app.tick_updates();
        assert!(
            app.notice
                .as_ref()
                .unwrap()
                .text
                .contains("9.9.0 is available")
        );
        // Once per version and launch.
        app.notice = None;
        app.updates.announce_pending = Some("9.9.0".into());
        app.tick_updates();
        assert!(app.notice.is_none());
        // Skipping hides it from the menu title too, for that version only.
        app.updates.scene = true;
        assert!(
            app.updates_menu_item()
                .title
                .starts_with("Update Available: 9.9.0")
        );
        let _ = app.updates_input(Input::Skip);
        assert_eq!(app.updates_menu_item().title, "Check for Updates…");
        found(&mut app, "9.9.1");
        assert!(app.updates_menu_item().title.contains("9.9.1"));
        // A version this build already has is never announced.
        found(&mut app, check::current());
        assert_eq!(app.updates.announce(), None);
    }

    #[test]
    fn every_scene_draws_without_fetching() {
        for word in [
            "updates",
            "updates-skipped",
            "updates-current",
            "updates-failed",
            "updates-checking",
            "updates-managed",
            "updates-none",
        ] {
            let mut app = bench();
            assert!(app.scene_updates(word, false), "{word}");
            assert!(app.updates.open);
            let _ = app.updates_sheet(&style::LIGHT, 600.0);
            let _ = app.updates_settings_section(&style::LIGHT);
            assert_eq!(app.updates.started, 0);
        }
        let mut app = bench();
        app.settings_open = true;
        assert!(app.scene_updates("updates-current", false));
        assert!(!app.updates.open, "Settings shows instead");
        assert!(!app.scene_updates("updatesx", false));
    }
}
