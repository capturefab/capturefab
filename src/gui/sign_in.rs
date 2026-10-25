//! "Sign in to camera": the sheet that opens when an rtsp://, rtsps:// or
//! ONVIF camera asks for a user name and password, and the background jobs
//! that save or forget a sign-in in the OS credential store.
//!
//! The password has one owner at each step: the draft while typing; at
//! Submit the `CameraLogin` moved into the connect command, plus a copy in
//! `Pending` only when it is to be remembered; after a successful connect
//! that copy moves into the save job, which drops it. A failed connect drops
//! the `Pending`, and with it the copy. Every copy zeroizes on drop. The
//! iced thread never reads the credential store; it reads only the index.
use super::secret::SecretDraft;
use super::*;
use crate::logins::{self, CameraLogin, Logins};
use iced::widget::column;

pub(super) const REJECTED: &str =
    "That user name and password didn't work. Check them and try again.";

#[derive(Debug)]
pub(super) struct SignIn {
    /// The credential-free address to connect, and to save a sign-in under.
    pub(super) target: String,
    pub(super) user: String,
    pub(super) password: SecretDraft,
    pub(super) remember: bool,
    pub(super) error: Option<String>,
    /// A sign-in saved on this computer stopped working.
    pub(super) saved_rejected: bool,
    /// A connect with this sign-in is on its way.
    pub(super) pending: bool,
    /// Focus a field on the next tick, as the sheet opens.
    pub(super) focus: bool,
    /// The password field was edited since the sheet opened or the last
    /// attempt. After a failure Sign in waits for it, so a stray Return
    /// never resends an empty password to a camera that may lock the account.
    pub(super) password_touched: bool,
}

impl SignIn {
    pub(super) fn new(target: String, user: Option<String>) -> Self {
        Self {
            target,
            user: user.unwrap_or_default(),
            password: SecretDraft::default(),
            remember: true,
            error: None,
            saved_rejected: false,
            pending: false,
            focus: true,
            password_touched: false,
        }
    }
    pub(super) fn ready(&self) -> bool {
        let retry = self.error.is_some() || self.saved_rejected;
        !self.user.trim().is_empty() && !self.pending && (!retry || self.password_touched)
    }
}

#[derive(Debug, Clone)]
pub(super) enum Input {
    User(String),
    Password(SecretDraft),
    Remember(bool),
    Submit,
    Cancel,
}

/// What a background save or forget reported.
pub(super) enum Reply {
    Saved {
        address: String,
        user: String,
        result: Result<(), String>,
    },
    Forgotten {
        address: String,
        /// Forgotten with its recent on the welcome screen, not only the
        /// sign-in from Settings.
        from_recent: bool,
        result: Result<bool, String>,
    },
}

/// Where a remembered password is kept, by OS.
fn store_caption() -> &'static str {
    if cfg!(target_os = "macos") {
        "Saved in your Keychain."
    } else if cfg!(windows) {
        "Saved in Windows Credential Manager."
    } else {
        "Saved in your system keyring."
    }
}

/// The camera as the sheet names it: without scheme, default port or the
/// default ONVIF path, and never with credentials.
fn sign_in_place(target: &str) -> String {
    match logins::key(target) {
        Ok(key) => logins::short(&key.address),
        Err(_) => short_address(&crate::media::redact_url(target)),
    }
}

fn spawn(work: impl FnOnce() -> Reply + Send + 'static) -> std::io::Result<Receiver<Reply>> {
    let (sender, receiver) = mpsc::channel();
    std::thread::Builder::new()
        .name("capturefab-camera-sign-in".into())
        .spawn(move || {
            let _ = sender.send(work());
        })?;
    Ok(receiver)
}

impl Workbench {
    /// Saved sign-ins by address, from the index alone (a small file).
    pub(super) fn load_saved_logins(&mut self) {
        if self.scene_logins {
            return;
        }
        self.saved_logins = Logins::system()
            .list()
            .map(|entries| {
                entries
                    .into_iter()
                    .map(|entry| (entry.address, entry.user))
                    .collect()
            })
            .unwrap_or_default();
    }

    /// The address a sign-in for `target` goes under: the target itself, or
    /// the ONVIF address of a discovered camera it names by ID or serial.
    pub(super) fn login_target(&self, target: &str) -> String {
        if logins::key(target).is_ok() {
            return target.to_owned();
        }
        self.snapshot
            .devices
            .iter()
            .chain(self.snapshot.cameras.iter().map(|camera| &camera.info))
            .find(|device| {
                crate::onvif::is_source(&device.id)
                    && (device.id == target || device.serial == target)
            })
            .map_or_else(|| target.to_owned(), |device| device.id.clone())
    }

    /// The saved user for a target, when it has a saved sign-in.
    pub(super) fn saved_login_user(&self, target: &str) -> Option<&str> {
        let key = logins::key(&self.login_target(target)).ok()?;
        self.saved_logins.get(&key.address).map(String::as_str)
    }

    /// A user name to suggest: this address's saved one, else one saved for
    /// the same scheme, host and port. Never a password.
    fn suggested_user(&self, target: &str) -> Option<String> {
        if let Some(user) = self.saved_login_user(target) {
            return Some(user.to_owned());
        }
        let key = logins::key(target).ok()?;
        let origin = |address: &str| {
            let start = address.find("://").map_or(0, |n| n + 3);
            let end = address[start..]
                .find('/')
                .map_or(address.len(), |n| start + n);
            address[..end].to_owned()
        };
        let wanted = origin(&key.address);
        self.saved_logins
            .iter()
            .find(|(address, _)| origin(address) == wanted)
            .map(|(_, user)| user.clone())
    }

    /// A connect came back asking for, or refusing, a sign-in.
    pub(super) fn sign_in_needed(&mut self, pending: &Pending, code: &str) {
        let Some(target) = &pending.target else {
            return;
        };
        let target = self.login_target(target);
        let rejected = code == "auth_rejected";
        let user = match self.sign_in.take() {
            Some(open) if open.target == target => Some(open.user),
            _ => pending
                .sent_user
                .clone()
                .or_else(|| self.suggested_user(&target)),
        };
        let mut sheet = SignIn::new(target, user);
        sheet.saved_rejected = rejected && !pending.sent_login;
        sheet.error = (rejected && pending.sent_login).then(|| REJECTED.to_owned());
        self.sign_in = Some(sheet);
    }

    pub(super) fn sign_in_input(&mut self, input: Input) -> Task<Message> {
        let Some(sheet) = &mut self.sign_in else {
            return Task::none();
        };
        match input {
            Input::User(user) if !sheet.pending => sheet.user = user,
            Input::Password(password) if !sheet.pending => {
                sheet.password = password;
                sheet.password_touched = true;
            }
            Input::Remember(remember) if !sheet.pending => sheet.remember = remember,
            Input::Submit if sheet.ready() => {
                let login = CameraLogin::new(sheet.user.trim(), sheet.password.as_str());
                // The draft is consumed here, whatever comes next.
                sheet.password = SecretDraft::default();
                sheet.password_touched = false;
                match login {
                    Ok(login) => {
                        sheet.pending = true;
                        sheet.error = None;
                        let remember = sheet.remember
                            && logins::can_remember()
                            && prefs::storable(&sheet.target);
                        let target = sheet.target.clone();
                        self.connect_with(target, Some(login), remember);
                    }
                    Err(_) => {
                        sheet.error = Some(
                            "User names and passwords can't contain control characters.".into(),
                        )
                    }
                }
            }
            Input::Cancel => {
                // Cancelling while signing in lets the connect finish, but
                // drops the copy to remember and keeps the sheet closed.
                if sheet.pending {
                    let target = sheet.target.clone();
                    let mine: Vec<usize> = (0..self.pending.len())
                        .filter(|&i| {
                            let pending = &self.pending[i];
                            pending.job == Job::Connect
                                && pending
                                    .target
                                    .as_deref()
                                    .is_some_and(|t| self.login_target(t) == target)
                        })
                        .collect();
                    for i in mine {
                        self.pending[i].login = None;
                        self.pending[i].sign_in_cancelled = true;
                    }
                }
                self.sign_in = None;
            }
            _ => {}
        }
        Task::none()
    }

    /// After a connect that sent a sign-in to remember: save it off the
    /// event loop. Consumes the only remaining copy.
    pub(super) fn save_sign_in(&mut self, target: &str, login: CameraLogin) {
        let Ok(key) = logins::key(target) else {
            return;
        };
        let address = key.address;
        let user = login.user.clone();
        let job = spawn(move || {
            let result = Logins::system()
                .save(&address, &login)
                .map(|_| ())
                .map_err(|error| format!("{error:#}"));
            drop(login);
            Reply::Saved {
                address,
                user,
                result,
            }
        });
        match job {
            Ok(job) => self.login_jobs.push(job),
            Err(error) => self.set_notice(
                format!("Connected, but the sign-in couldn't be saved: {error}"),
                None,
                Level::Warning,
            ),
        }
    }

    /// Forget a saved sign-in off the event loop.
    pub(super) fn forget_sign_in(&mut self, address: String, from_recent: bool) {
        let job = spawn(move || {
            let result = Logins::system()
                .forget(&address)
                .map_err(|error| format!("{error:#}"));
            Reply::Forgotten {
                address,
                from_recent,
                result,
            }
        });
        if let Ok(job) = job {
            self.login_jobs.push(job);
        }
    }

    /// Collect finished save and forget jobs; from `tick()`.
    pub(super) fn tick_sign_in(&mut self) {
        let mut index = 0;
        while index < self.login_jobs.len() {
            let reply = match self.login_jobs[index].try_recv() {
                Ok(reply) => reply,
                Err(TryRecvError::Empty) => {
                    index += 1;
                    continue;
                }
                Err(TryRecvError::Disconnected) => {
                    self.login_jobs.remove(index);
                    continue;
                }
            };
            self.login_jobs.remove(index);
            match reply {
                Reply::Saved {
                    address,
                    user,
                    result: Ok(()),
                } => {
                    let host = logins::short(&address);
                    let host = host.split('/').next().unwrap_or(&host).to_owned();
                    self.saved_logins.insert(address, user);
                    self.set_notice(
                        format!("Signed in to {host}. Saved on this computer."),
                        None,
                        Level::Done,
                    );
                }
                Reply::Saved {
                    result: Err(error), ..
                } => self.set_notice(
                    format!("Connected, but the sign-in couldn't be saved: {error}"),
                    None,
                    Level::Warning,
                ),
                Reply::Forgotten {
                    address,
                    from_recent,
                    result,
                } => {
                    self.saved_logins.remove(&address);
                    match result {
                        Ok(_) => {
                            let host = logins::short(&address);
                            let host = host.split('/').next().unwrap_or(&host).to_owned();
                            self.set_notice(
                                if from_recent {
                                    format!("Forgot {host} and its saved sign-in.")
                                } else {
                                    format!("Removed the saved sign-in for {host}.")
                                },
                                None,
                                Level::Done,
                            );
                        }
                        Err(error) => self.set_notice(
                            "Could not forget the saved sign-in",
                            Some(error),
                            Level::Error,
                        ),
                    }
                    self.load_saved_logins();
                }
            }
        }
    }

    pub(super) fn sign_in_sheet(&self, p: &'static Palette) -> Option<Element<'_, Message>> {
        let sheet = self.sign_in.as_ref()?;
        debug_assert!(logins::can_remember(), "every GUI build has the keychain");
        let pending = sheet.pending;
        let send = |input| Message::SignIn(input);
        let place = sign_in_place(&sheet.target);
        let bold = |value: String| iced::widget::span(value).font(style::SEMIBOLD);
        let plain = |value: &'static str| iced::widget::span(value.to_owned());
        let spans: Vec<iced::widget::text::Span<'_, ()>> = if sheet.saved_rejected {
            vec![
                plain("The saved sign-in for "),
                bold(place.clone()),
                plain(" no longer works. Enter the current password."),
            ]
        } else {
            vec![
                bold(place.clone()),
                plain(" needs a user name and password."),
            ]
        };
        let lead = iced::widget::rich_text(spans)
            .size(style::BODY)
            .color(p.text)
            .width(Fill);
        // Labels sit above the fields, so no placeholder repeats them.
        let user = text_input("", &sheet.user)
            .id("sign-in-user")
            .name("User name")
            .on_input_maybe((!pending).then_some(|value| Message::SignIn(Input::User(value))))
            .on_submit_maybe(sheet.ready().then_some(send(Input::Submit)))
            .padding([6, 10])
            .size(style::BODY)
            .style(style::input);
        let password = text_input("", sheet.password.as_str())
            .id("sign-in-password")
            .name("Password")
            .description("Sent only to this camera. Never saved in app preferences.")
            .secure(true)
            .on_input_maybe(
                (!pending)
                    .then_some(|value| Message::SignIn(Input::Password(SecretDraft::from(value)))),
            )
            .on_submit_maybe(sheet.ready().then_some(send(Input::Submit)))
            .padding([6, 10])
            .size(style::BODY)
            .style(style::input);
        fn field<'a>(
            label: &'static str,
            input: Element<'a, Message>,
            p: &'static Palette,
        ) -> Element<'a, Message> {
            column![
                text(label)
                    .size(style::SMALL)
                    .font(style::MEDIUM)
                    .color(p.secondary),
                input
            ]
            .spacing(5)
            .into()
        }
        let mut body = column![
            lead,
            field("User name", user.into(), p),
            field("Password", password.into(), p),
        ]
        .spacing(14);
        if let Some(error) = &sheet.error {
            body = body.push(
                text(error.as_str())
                    .size(style::SMALL)
                    .color(Level::Error.color(p)),
            );
        }
        if prefs::storable(&sheet.target) {
            body = body.push(
                column![
                    checkbox_maybe(
                        "Remember on this computer",
                        sheet.remember,
                        (!pending).then_some(|value| Message::SignIn(Input::Remember(value))),
                    ),
                    text(if sheet.remember {
                        store_caption()
                    } else {
                        "Used for this connection only."
                    })
                    .size(style::SMALL)
                    .color(p.secondary),
                ]
                .spacing(4),
            );
        } else {
            body = body.push(
                text("This address carries a token, so it can't be remembered.")
                    .size(style::SMALL)
                    .color(p.secondary),
            );
        }
        let cancel = button(text("Cancel").size(style::BODY))
            .name("Cancel sign-in")
            .padding([7, 18])
            .style(style::secondary)
            .on_press(send(Input::Cancel));
        let label: Element<'_, Message> = if pending {
            row![
                icon::spinner(12.0, p.secondary, self.spin()),
                text("Signing in…").size(style::BODY).font(style::MEDIUM),
            ]
            .spacing(7)
            .align_y(Alignment::Center)
            .into()
        } else {
            text("Sign in").size(style::BODY).font(style::MEDIUM).into()
        };
        let submit = button(label)
            .name(if pending { "Signing in…" } else { "Sign in" })
            .padding([7, 18])
            .style(style::primary)
            .on_press_maybe(sheet.ready().then_some(send(Input::Submit)));
        let footer = row![space().width(Fill), cancel, submit]
            .spacing(10)
            .align_y(Alignment::Center);
        Some(
            sheet_frame("Sign in to camera", send(Input::Cancel), body, p)
                .mark(Icon::Lock)
                .size(420.0, self.sign_in_height())
                .footer(footer)
                .default_action(sheet.ready().then_some(send(Input::Submit)))
                .into(),
        )
    }

    fn sign_in_height(&self) -> f32 {
        (self.height - 64.0).clamp(240.0, 460.0)
    }

    /// Focus the user name, or the password once a user name is known.
    pub(super) fn focus_sign_in(&self) -> Task<Message> {
        match &self.sign_in {
            Some(sheet) if sheet.user.is_empty() => operation::focus("sign-in-user"),
            Some(_) => operation::focus("sign-in-password"),
            None => Task::none(),
        }
    }

    /// Settings: one row per saved sign-in, each with Forget.
    pub(super) fn sign_ins_section(&self, p: &'static Palette) -> Element<'_, Message> {
        let mut section = column![heading("Camera sign-ins", p)].spacing(9);
        if self.saved_logins.is_empty() {
            return section
                .push(
                    text("Cameras you sign in to and choose to remember appear here.")
                        .size(style::SMALL)
                        .color(p.secondary),
                )
                .into();
        }
        for (address, user) in &self.saved_logins {
            let place = logins::short(address);
            // The recent's label, when one maps to this sign-in, is what
            // the person knows the camera by.
            let label = self
                .recent
                .iter()
                .find(|recent| {
                    logins::key(&self.login_target(&recent.target))
                        .is_ok_and(|key| &key.address == address)
                })
                .map(|recent| recent.label.clone())
                .filter(|label| !label.trim().is_empty() && *label != place);
            let (title, caption) = match label {
                Some(label) => (label, format!("{place} · Signed in as {user}")),
                None => (place.clone(), format!("Signed in as {user}")),
            };
            let forget = button(text("Forget").size(style::BODY))
                .name(format!("Forget the sign-in for {place}"))
                .padding([4, 12])
                .style(style::secondary)
                .on_press(Message::ForgetSignIn(address.clone()));
            section = section.push(
                row![
                    column![
                        one_line(title, style::BODY, style::MEDIUM, p.text).width(Fill),
                        one_line(caption, style::CAPTION, style::SANS, p.secondary).width(Fill),
                    ]
                    .spacing(2)
                    .width(Fill),
                    forget,
                ]
                .spacing(12)
                .align_y(Alignment::Center),
            );
        }
        section.into()
    }

    /// Take a screenshot scene word for the sign-in sheet and saved
    /// sign-ins; never touches the credential store. Words:
    /// - `sign-in`: the empty sheet for an RTSP camera
    /// - `sign-in-rejected`: `admin` prefilled, with the error line
    /// - `sign-in-saved-rejected`: a saved sign-in that stopped working
    /// - `sign-in-pending`: "Signing in…"
    /// - `signed-in-recent`: a remembered camera among the recents
    /// - `settings-sign-ins`: Settings with two saved sign-ins
    pub(super) fn scene_sign_in(&mut self, word: &str, late: bool) -> bool {
        const TARGET: &str = "rtsp://192.168.10.60:554/Streaming/Channels/101";
        let words = [
            "sign-in",
            "sign-in-rejected",
            "sign-in-saved-rejected",
            "sign-in-pending",
            "signed-in-recent",
            "settings-sign-ins",
        ];
        if !words.contains(&word) {
            return false;
        }
        if late {
            return true;
        }
        self.scene_logins = true;
        match word {
            "signed-in-recent" | "settings-sign-ins" => {
                self.saved_logins = [
                    ("rtsp://192.168.10.60:554/Streaming/Channels/101", "admin"),
                    (
                        "onvif:http://192.168.10.61:80/onvif/device_service",
                        "operator",
                    ),
                ]
                .into_iter()
                .map(|(address, user)| (address.to_owned(), user.to_owned()))
                .collect();
                if word == "settings-sign-ins" {
                    self.settings_open = true;
                } else {
                    let mut prefs = Prefs {
                        recent: std::mem::take(&mut self.recent),
                        ..Prefs::default()
                    };
                    prefs.remember(Recent {
                        target: "rtsp://192.168.10.60/Streaming/Channels/101".into(),
                        label: "Loading dock".into(),
                        detail: "Media · rtsp://192.168.10.60/Streaming/Channels/101".into(),
                        transport: crate::types::Transport::Media,
                    });
                    self.recent = prefs.recent;
                }
            }
            _ => {
                let mut sheet = SignIn::new(
                    TARGET.into(),
                    (word != "sign-in").then(|| "admin".to_owned()),
                );
                match word {
                    "sign-in-rejected" => sheet.error = Some(REJECTED.into()),
                    "sign-in-saved-rejected" => sheet.saved_rejected = true,
                    "sign-in-pending" => {
                        sheet.pending = true;
                        sheet.password = SecretDraft::from("example-password".to_owned());
                    }
                    _ => {}
                }
                self.sign_in = Some(sheet);
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bench() -> Workbench {
        let mut bench = Workbench::new(SessionHandle::new(), "test".into(), false, None);
        bench.scene_logins = true;
        bench.saved_logins.clear();
        bench
    }
    fn connecting(target: &str, sent_login: bool) -> Pending {
        Pending {
            target: Some(target.into()),
            sent_login,
            ..Pending::unanswered(Job::Connect)
        }
    }
    const TARGET: &str = "rtsp://cam.local/live";

    #[test]
    fn sign_in_requests_open_the_sheet_instead_of_a_failure() {
        let mut bench = bench();
        let required = logins::auth_error(TARGET, false);
        bench.settle(&connecting(TARGET, false), &Err(required));
        let sheet = bench.sign_in.as_ref().expect("the sheet opens");
        assert_eq!(sheet.target, TARGET);
        assert!(sheet.error.is_none() && !sheet.saved_rejected);
        assert!(bench.side.connect_failures.is_empty());
        assert!(bench.notice.is_none(), "no red notice");

        let rejected = || logins::auth_error(TARGET, true);
        bench.sign_in.as_mut().unwrap().user = "admin".into();
        bench.settle(&connecting(TARGET, true), &Err(rejected()));
        let sheet = bench.sign_in.as_ref().unwrap();
        assert_eq!(sheet.error.as_deref(), Some(REJECTED));
        assert_eq!(sheet.user, "admin", "the user name stays");
        // After a refusal, Return waits for the password to be edited.
        assert!(!sheet.ready());
        let _ = bench.sign_in_input(Input::Password(SecretDraft::default()));
        assert!(bench.sign_in.as_ref().unwrap().ready());

        bench.sign_in = None;
        bench
            .saved_logins
            .insert("rtsp://cam.local:554/live".into(), "viewer".into());
        bench.settle(&connecting(TARGET, false), &Err(rejected()));
        let sheet = bench.sign_in.as_ref().unwrap();
        assert!(sheet.saved_rejected && sheet.error.is_none());
        assert_eq!(sheet.user, "viewer", "prefilled from the saved sign-in");

        // Any other failure still shows on the card.
        bench.sign_in = None;
        bench.settle(
            &connecting(TARGET, false),
            &Err(anyhow::anyhow!("timed out")),
        );
        assert!(bench.sign_in.is_none());
        assert!(bench.side.connect_failures.contains_key(TARGET));
        bench.handle.shutdown();
    }

    #[test]
    fn submitting_sends_the_sign_in_beside_the_address_and_clears_the_draft() {
        let mut bench = bench();
        bench.sign_in = Some(SignIn::new(TARGET.into(), None));
        // Return does nothing without a user name.
        let _ = bench.sign_in_input(Input::Submit);
        assert!(bench.pending.is_empty());
        let _ = bench.sign_in_input(Input::User("admin".into()));
        let _ = bench.sign_in_input(Input::Password(SecretDraft::from("pw".to_owned())));
        let _ = bench.sign_in_input(Input::Remember(false));
        let _ = bench.sign_in_input(Input::Submit);
        let sheet = bench.sign_in.as_ref().unwrap();
        assert!(sheet.pending && sheet.password.as_str().is_empty());
        let pending = bench.pending.last().unwrap();
        assert_eq!(pending.target.as_deref(), Some(TARGET));
        assert!(pending.sent_login && pending.login.is_none());
        // While pending, fields and Submit do nothing.
        let _ = bench.sign_in_input(Input::User("other".into()));
        assert_eq!(bench.sign_in.as_ref().unwrap().user, "admin");
        let _ = bench.sign_in_input(Input::Cancel);
        assert!(bench.sign_in.is_none());

        bench.pending.clear();
        bench.sign_in = Some(SignIn::new(
            "rtsp://other.local/live".into(),
            Some("admin".into()),
        ));
        let _ = bench.sign_in_input(Input::Password(SecretDraft::from("pw2".to_owned())));
        let _ = bench.sign_in_input(Input::Submit);
        let pending = bench.pending.last().unwrap();
        assert_eq!(
            pending.login.as_ref().map(CameraLogin::expose),
            Some("pw2"),
            "kept only to be remembered"
        );
        bench.handle.shutdown();
    }

    #[test]
    fn a_successful_remembered_sign_in_is_saved_once() {
        let mut bench = bench();
        for (remember, jobs) in [(false, 0), (true, 1)] {
            let (sender, receiver) = mpsc::channel();
            sender
                .send(Ok(serde_json::json!({"connected": null})))
                .unwrap();
            bench.pending.push(Pending {
                receiver,
                target: Some("rtsp://saved-gui.invalid/live".into()),
                sent_login: true,
                login: remember.then(|| CameraLogin::new("admin", "gui-pw").unwrap()),
                ..Pending::unanswered(Job::Connect)
            });
            bench.poll();
            assert_eq!(bench.login_jobs.len(), jobs);
        }
        bench.handle.shutdown();
    }

    #[test]
    fn typed_credentials_are_split_out_and_used_once() {
        let mut bench = bench();
        bench.address = "rtsp://typed-user:typed-pw@cam.local/x".into();
        bench.connect(bench.address.clone());
        assert_eq!(bench.address, "rtsp://cam.local/x");
        let pending = bench.pending.pop().unwrap();
        assert_eq!(pending.target.as_deref(), Some("rtsp://cam.local/x"));
        assert!(pending.sent_login && pending.login.is_none());
        // The recent is the credential-free address, so prefs hold neither
        // the user name nor the password.
        let mut camera = crate::transport::simulator::info();
        camera.id = "rtsp://cam.local/x".into();
        camera.transport = crate::types::Transport::Media;
        camera.address = Some("rtsp://cam.local/x".into());
        bench.settle(&pending, &Ok(serde_json::json!({"connected": camera})));
        assert_eq!(bench.recent[0].target, "rtsp://cam.local/x");
        let prefs = serde_json::to_string(&Prefs {
            recent: bench.recent.clone(),
            ..Prefs::default()
        })
        .unwrap();
        assert!(
            !prefs.contains("typed-pw") && !prefs.contains("typed-user"),
            "{prefs}"
        );
        bench.handle.shutdown();
    }

    #[test]
    fn other_urls_keep_their_credentials_for_ffmpeg() {
        let mut bench = bench();
        let typed = "http://u:p@cam.local/mjpg/video.mjpg";
        bench.address = typed.into();
        bench.connect(typed.into());
        assert_eq!(bench.address, typed);
        let pending = bench.pending.pop().expect("a Connect is sent");
        assert_eq!(pending.target.as_deref(), Some(typed));
        assert!(!pending.sent_login && pending.sent_user.is_none());
        assert!(bench.side.connect_failures.is_empty());
        bench.handle.shutdown();
    }

    #[test]
    fn a_refused_typed_address_keeps_only_its_user_name() {
        let mut bench = bench();
        bench.connect("rtsp://admin:wrong@cam.local/live".into());
        let pending = bench.pending.pop().unwrap();
        assert_eq!(pending.sent_user.as_deref(), Some("admin"));
        bench.settle(&pending, &Err(logins::auth_error(TARGET, true)));
        let sheet = bench.sign_in.as_ref().unwrap();
        assert_eq!(sheet.user, "admin");
        assert!(sheet.password.as_str().is_empty());
        bench.handle.shutdown();
    }

    #[test]
    fn other_failures_show_in_the_open_sheet() {
        let mut bench = bench();
        bench.sign_in = Some(SignIn::new(TARGET.into(), Some("admin".into())));
        let _ = bench.sign_in_input(Input::Submit);
        let pending = bench.pending.pop().unwrap();
        bench.settle(&pending, &Err(anyhow::anyhow!("camera timed out")));
        let sheet = bench.sign_in.as_ref().expect("the sheet stays");
        assert!(!sheet.pending);
        assert!(sheet.error.as_deref().unwrap().contains("timed out"));
        bench.handle.shutdown();
    }

    #[test]
    fn cancelling_while_signing_in_saves_nothing_and_stays_closed() {
        let mut bench = bench();
        for refused in [false, true] {
            bench.sign_in = Some(SignIn::new(TARGET.into(), Some("admin".into())));
            let _ = bench.sign_in_input(Input::Submit);
            assert!(bench.pending.last().unwrap().login.is_some());
            let _ = bench.sign_in_input(Input::Cancel);
            assert!(bench.sign_in.is_none());
            let mut pending = bench.pending.pop().unwrap();
            assert!(pending.sign_in_cancelled && pending.login.is_none());
            let (sender, receiver) = mpsc::channel();
            sender
                .send(if refused {
                    Err(logins::auth_error(TARGET, true))
                } else {
                    Ok(serde_json::json!({"connected": null}))
                })
                .unwrap();
            pending.receiver = receiver;
            bench.pending.push(pending);
            bench.poll();
            assert!(bench.login_jobs.is_empty(), "nothing is saved");
            assert!(bench.sign_in.is_none(), "the sheet does not come back");
        }
        bench.handle.shutdown();
    }

    #[test]
    fn sign_in_addresses_read_the_same_everywhere() {
        assert_eq!(
            sign_in_place("rtsp://192.168.10.60:554/Streaming/Channels/101"),
            "192.168.10.60/Streaming/Channels/101"
        );
        assert_eq!(sign_in_place("onvif://192.168.10.61"), "192.168.10.61");
        assert_eq!(
            logins::short("onvif:http://192.168.10.61:80/onvif/device_service"),
            "192.168.10.61"
        );
    }

    #[test]
    fn forgetting_a_remembered_recent_forgets_its_sign_in() {
        let mut bench = bench();
        bench
            .saved_logins
            .insert("rtsp://cam.local:554/live".into(), "admin".into());
        let _ = bench.update(Message::Forget("rtsp://cam.local/live".into()));
        assert_eq!(bench.login_jobs.len(), 1);
        let _ = bench.update(Message::Forget("rtsp://plain.local/live".into()));
        assert_eq!(bench.login_jobs.len(), 1);
        bench.handle.shutdown();
    }

    #[test]
    fn every_sheet_state_renders() {
        let mut bench = bench();
        for word in [
            "sign-in",
            "sign-in-rejected",
            "sign-in-saved-rejected",
            "sign-in-pending",
        ] {
            assert!(bench.scene_sign_in(word, false));
            assert!(bench.sign_in_sheet(Palette::of(false)).is_some());
        }
        assert!(bench.scene_sign_in("settings-sign-ins", false));
        let _ = bench.sign_ins_section(Palette::of(true));
        assert!(!bench.scene_sign_in("other", false));
        bench.handle.shutdown();
    }
}
