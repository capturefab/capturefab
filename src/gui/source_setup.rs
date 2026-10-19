//! Shared protected-source editor for Quick Command, tasks and live signals.
//! Drafts are local; only an explicit worker binding can add a source to scope.
use super::{secret::SecretDraft, *};
use crate::{
    signals::auth::{ProfileDraft, ProfileInfo, ProfileRef, SecretKind, SecretSource},
    tools::context::DataSourceRef,
};
use iced::widget::column;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
    mpsc::TryRecvError,
};
#[cfg(test)]
#[path = "source_setup_tests.rs"]
pub(super) mod tests;
#[path = "source_setup_worker.rs"]
mod worker;
pub(super) use worker::{Native, Services};
pub(super) const UNCOMMITTED_DRAFT: &str = "Save this source profile and choose Use source, or close the editor to keep the sources already selected.";

#[derive(Clone)]
pub(super) enum Input {
    Open(bool),
    Select(ProfileRef),
    New,
    Edit,
    Name(String),
    Endpoint(String),
    Username(String),
    Authentication(SecretKind),
    Environment(String),
    Password(SecretDraft),
    Save,
    Use,
    Delete,
    Reload,
}
impl fmt::Debug for Input {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Open(open) => f.debug_tuple("SourceProfiles").field(open).finish(),
            Self::Authentication(kind) => f.debug_tuple("Authentication").field(kind).finish(),
            _ => f.write_str("SourceProfileInput([redacted])"),
        }
    }
}
pub(super) enum Effect {
    Bound(DataSourceRef),
    Invalidate(String),
}
#[derive(Clone, Copy)]
pub(super) enum Purpose {
    Approve,
    Log,
}

#[derive(Clone, PartialEq, Eq)]
struct Choice {
    profile: ProfileRef,
    label: String,
}
impl fmt::Display for Choice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.label)
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
struct Authentication(SecretKind);
impl fmt::Display for Authentication {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self.0 {
            SecretKind::None => "No authentication",
            SecretKind::Keychain => "OS credential store",
            SecretKind::Environment => "Environment variable",
        })
    }
}

pub(super) struct State {
    pub open: bool,
    pub profiles: Vec<ProfileInfo>,
    pub selected: Option<ProfileRef>,
    pub editing: bool,
    pub name: String,
    pub endpoint: String,
    pub username: String,
    pub authentication: SecretKind,
    pub environment: String,
    password: SecretDraft,
    pub error: Option<String>,
    pub status: Option<String>,
    pub pending: bool,
    loaded: bool,
    synthetic: bool,
    generation: Arc<AtomicU64>,
    services: Arc<dyn Services>,
    worker: Option<worker::Worker>,
}
impl fmt::Debug for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SourceSetup")
            .field("profiles", &self.profiles.len())
            .field("pending", &self.pending)
            .finish_non_exhaustive()
    }
}
impl Default for State {
    fn default() -> Self {
        Self::with_services(Arc::new(worker::Unavailable))
    }
}
impl State {
    pub fn with_services(services: Arc<dyn Services>) -> Self {
        Self {
            open: false,
            profiles: vec![],
            selected: None,
            editing: false,
            name: String::new(),
            endpoint: String::new(),
            username: String::new(),
            authentication: SecretKind::None,
            environment: String::new(),
            password: SecretDraft::default(),
            error: None,
            status: None,
            pending: false,
            loaded: false,
            synthetic: false,
            generation: Arc::new(AtomicU64::new(0)),
            services,
            worker: None,
        }
    }
    pub fn configure(&mut self, services: Arc<dyn Services>) {
        self.services = services;
    }
    pub fn set_synthetic(&mut self, synthetic: bool) {
        self.synthetic = synthetic;
    }
    pub fn clear_secret(&mut self) {
        self.password = SecretDraft::default();
        self.generation.fetch_add(1, Ordering::SeqCst);
    }
    pub fn has_uncommitted_draft(&self) -> bool {
        self.open
            && self.editing
            && (!self.name.is_empty()
                || !self.endpoint.is_empty()
                || !self.username.is_empty()
                || !self.environment.is_empty()
                || !self.password.as_str().is_empty())
    }
    pub fn reset(&mut self) {
        self.clear_secret();
        let services = self.services.clone();
        *self = Self::with_services(services);
    }
    pub fn fixture(&mut self) {
        self.synthetic = true;
        self.loaded = true;
        self.open = true;
        self.editing = true;
        self.name = "line1-mqtt".into();
        self.endpoint = "mqtts://broker.example/line1/temperature".into();
        self.username = "line1-reader".into();
        self.authentication = SecretKind::Keychain;
        self.password = "fixture-only-password".to_string().into();
    }
    fn draft(&self) -> ProfileDraft {
        ProfileDraft {
            name: self.name.trim().into(),
            endpoint: self.endpoint.trim().into(),
            username: (!self.username.trim().is_empty()).then(|| self.username.trim().into()),
            secret: match self.authentication {
                SecretKind::None => SecretSource::None,
                SecretKind::Keychain => SecretSource::Keychain,
                SecretKind::Environment => SecretSource::Environment {
                    variable: self.environment.trim().into(),
                },
            },
        }
    }
    pub fn draft_error(&self) -> Option<String> {
        self.draft()
            .validate()
            .err()
            .map(|error| crate::tools::redact::for_output_text(&error.to_string()))
    }
    fn enqueue(&mut self, work: worker::Work) {
        if self.synthetic {
            return;
        }
        if self.worker.is_none() {
            match worker::start(self.services.clone(), self.generation.clone()) {
                Ok(worker) => self.worker = Some(worker),
                Err(error) => {
                    self.error = Some(error);
                    return;
                }
            }
        }
        let generation = self.generation.load(Ordering::SeqCst);
        if self
            .worker
            .as_ref()
            .unwrap()
            .send
            .send((generation, work))
            .is_ok()
        {
            self.pending = true;
            self.error = None;
        } else {
            self.worker_stopped();
        }
    }
    fn worker_stopped(&mut self) {
        self.worker = None;
        self.loaded = false;
        self.pending = false;
        // Reject replies collected immediately before the worker disconnected.
        // Reopening or refreshing starts a new worker with this newer fence.
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.error =
            Some("The source profile worker stopped. Reopen or refresh this editor.".into());
    }
    fn changed(&mut self) -> Vec<Effect> {
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.error = None;
        self.status = None;
        self.selected
            .as_ref()
            .map(|profile| vec![Effect::Invalidate(profile.id.clone())])
            .unwrap_or_default()
    }
    pub fn input(&mut self, input: Input, enabled: bool) -> Vec<Effect> {
        if let Input::Open(open) = input {
            self.open = open;
            if open && !self.loaded && !self.pending {
                self.enqueue(worker::Work::List);
            }
            if !open {
                self.clear_secret();
            }
            return vec![];
        }
        if !enabled || self.pending {
            return vec![];
        }
        match input {
            Input::Select(profile) => {
                self.clear_secret();
                self.selected = Some(profile);
                self.editing = false;
                self.error = None;
                self.status = None;
            }
            Input::New => {
                self.clear_secret();
                self.selected = None;
                self.name.clear();
                self.endpoint.clear();
                self.username.clear();
                self.environment.clear();
                self.authentication = SecretKind::None;
                self.editing = true;
                self.error = None;
                self.status = None;
            }
            Input::Edit => {
                if let Some(profile) = self.selected.clone() {
                    let effect = vec![Effect::Invalidate(profile.id.clone())];
                    self.changed();
                    self.enqueue(worker::Work::Edit(profile));
                    return effect;
                }
            }
            Input::Name(value) => {
                self.name = value;
                return self.changed();
            }
            Input::Endpoint(value) => {
                self.endpoint = value;
                return self.changed();
            }
            Input::Username(value) => {
                self.username = value;
                return self.changed();
            }
            Input::Authentication(kind) => {
                self.authentication = kind;
                self.password = SecretDraft::default();
                return self.changed();
            }
            Input::Environment(value) => {
                self.environment = value;
                return self.changed();
            }
            Input::Password(value) => {
                self.password = value;
                return self.changed();
            }
            Input::Save => {
                if let Some(error) = self.draft_error() {
                    self.error = Some(error);
                    return vec![];
                }
                if self.authentication == SecretKind::Keychain
                    && self.selected.is_none()
                    && self.password.as_str().is_empty()
                {
                    self.error =
                        Some("Enter the source password before saving this profile.".into());
                    return vec![];
                }
                let password = (!self.password.as_str().is_empty())
                    .then(|| std::mem::take(&mut self.password));
                self.enqueue(worker::Work::Save {
                    expected: self.selected.clone(),
                    draft: self.draft(),
                    password,
                });
            }
            Input::Use => {
                if !self.editing
                    && let Some(profile) = self.selected.clone()
                {
                    self.enqueue(worker::Work::Bind(profile));
                }
            }
            Input::Delete => {
                if let Some(profile) = self.selected.clone() {
                    let effect = vec![Effect::Invalidate(profile.id.clone())];
                    self.enqueue(worker::Work::Delete(profile));
                    return effect;
                }
            }
            Input::Reload => self.enqueue(worker::Work::List),
            Input::Open(_) => {}
        }
        vec![]
    }
    pub fn tick(&mut self) -> Vec<Effect> {
        let mut replies = vec![];
        let mut disconnected = false;
        if let Some(worker) = &self.worker {
            loop {
                match worker.receive.try_recv() {
                    Ok(reply) => replies.push(reply),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }
        if disconnected {
            self.worker_stopped();
        }
        let mut effects = vec![];
        for (generation, reply) in replies {
            self.pending = false;
            let current = generation == self.generation.load(Ordering::SeqCst);
            match reply {
                worker::Reply::Saved(expected, result) => {
                    if let Some(expected) = expected {
                        effects.push(Effect::Invalidate(expected.id));
                    }
                    match result {
                        Ok(info) => {
                            self.profiles
                                .retain(|profile| profile.profile.id != info.profile.id);
                            self.profiles.push(info.clone());
                            if current {
                                self.selected = Some(info.profile);
                                self.editing = false;
                                self.status=Some("Profile saved locally. Use source to load its credentials for this session.".into());
                            }
                        }
                        Err(error) if current => self.error = Some(error),
                        _ => {}
                    }
                }
                worker::Reply::Deleted(profile, result) => {
                    if result.is_ok() {
                        effects.push(Effect::Invalidate(profile.id.clone()));
                        self.profiles.retain(|info| info.profile.id != profile.id);
                        if current {
                            self.selected = None;
                            self.editing = false;
                            self.status = Some("Source profile removed.".into());
                        }
                    } else if current {
                        self.error = result.err();
                    }
                }
                worker::Reply::List(result) if current => match result {
                    Ok(profiles) => {
                        for previous in &self.profiles {
                            if !profiles
                                .iter()
                                .any(|profile| profile.profile == previous.profile)
                            {
                                effects.push(Effect::Invalidate(previous.profile.id.clone()));
                            }
                        }
                        if self.selected.as_ref().is_some_and(|selected| {
                            !profiles.iter().any(|profile| &profile.profile == selected)
                        }) {
                            self.password = SecretDraft::default();
                            if self.editing {
                                self.error=Some("This profile changed. Choose its latest saved version before editing.".into());
                            } else {
                                self.selected = None;
                            }
                        }
                        // Refresh metadata without hiding an active draft or
                        // dropping the revision pinned for its eventual save.
                        self.editing |= !self.loaded && profiles.is_empty();
                        self.profiles = profiles;
                        self.loaded = true;
                    }
                    Err(error) => self.error = Some(error),
                },
                worker::Reply::Edit(profile, result)
                    if current && self.selected.as_ref() == Some(&profile) =>
                {
                    match result {
                        Ok(draft) => {
                            self.name = draft.name;
                            self.endpoint = draft.endpoint;
                            self.username = draft.username.unwrap_or_default();
                            self.authentication = draft.secret.kind();
                            self.environment = match draft.secret {
                                SecretSource::Environment { variable } => variable,
                                _ => String::new(),
                            };
                            self.editing = true;
                            self.password = SecretDraft::default();
                        }
                        Err(error) => self.error = Some(error),
                    }
                }
                worker::Reply::Bound(result) if current => match result {
                    Ok(binding) => {
                        self.status = Some("Source credentials are ready for this session.".into());
                        effects.push(Effect::Bound(binding));
                    }
                    Err(error) => self.error = Some(error),
                },
                _ => {}
            }
        }
        if self.open && !self.loaded && !self.pending && self.error.is_none() {
            self.enqueue(worker::Work::List);
        }
        effects
    }
}

pub(super) fn view<'a>(
    state: &'a State,
    p: &'static Palette,
    enabled: bool,
    prefix: &'static str,
    purpose: Purpose,
) -> Element<'a, Input> {
    let editable = enabled && !state.pending;
    let mut body = column![widgets::disclosure(
        "Protected source profiles",
        Some(if state.pending {
            "Working locally…".into()
        } else {
            format!("{} saved", state.profiles.len())
        }),
        if state.open { 1.0 } else { 0.0 },
        Input::Open(!state.open),
        p
    )]
    .spacing(8);
    if !state.open {
        return body.into();
    }
    body = body.push(
        text(
            "Keep user names and passwords out of source URLs. Passwords stay out of app preferences and model requests.",
        )
        .size(style::SMALL)
        .color(p.secondary)
        .wrapping(text::Wrapping::WordOrGlyph),
    );
    body = body.push(
        button(text("Refresh profiles").size(style::SMALL))
            .name("Refresh local source profiles")
            .padding([4, 6])
            .style(style::plain)
            .on_press_maybe(editable.then_some(Input::Reload)),
    );
    let choices = state
        .profiles
        .iter()
        .map(|profile| Choice {
            profile: profile.profile.clone(),
            label: format!("{} · {}", profile.name, profile.protocol.label()),
        })
        .collect::<Vec<_>>();
    let selected = choices
        .iter()
        .find(|choice| Some(&choice.profile) == state.selected.as_ref())
        .cloned();
    if !choices.is_empty() {
        body = body.push(
            row![
                pick_list(choices, selected, |choice: Choice| Input::Select(
                    choice.profile
                ))
                .name("Saved source profile")
                .placeholder("Choose a saved source")
                .width(Fill)
                .text_size(style::BODY)
                .enabled(editable),
                button(text("New source").size(style::SMALL))
                    .name("Create protected source profile")
                    .padding([7, 10])
                    .style(style::secondary)
                    .on_press_maybe(editable.then_some(Input::New))
            ]
            .spacing(8),
        );
    } else if !state.editing {
        body = body.push(
            button(text("New source").size(style::SMALL))
                .name("Create protected source profile")
                .padding([7, 10])
                .style(style::secondary)
                .on_press_maybe(editable.then_some(Input::New)),
        );
    }
    if state.editing {
        let field = |label: &'static str, value: &'a str, kind: fn(String) -> Input| {
            column![
                text(label).size(style::SMALL).color(p.secondary),
                text_input::text_input(label, value)
                    .name(label)
                    .id(format!("{prefix}-{label}"))
                    .size(style::BODY)
                    .padding([7, 10])
                    .width(Fill)
                    .style(style::input)
                    .on_input_maybe(editable.then_some(kind))
            ]
            .spacing(4)
        };
        body = body
            .push(field("Source profile name", &state.name, Input::Name))
            .push(field("Source endpoint", &state.endpoint, Input::Endpoint))
            .push(field("Source user name", &state.username, Input::Username));
        body = body.push(
            column![
                text("Authentication").size(style::SMALL).color(p.secondary),
                pick_list(
                    vec![
                        Authentication(SecretKind::None),
                        Authentication(SecretKind::Keychain),
                        Authentication(SecretKind::Environment)
                    ],
                    Some(Authentication(state.authentication)),
                    |choice: Authentication| Input::Authentication(choice.0)
                )
                .name("Source authentication")
                .width(Fill)
                .text_size(style::BODY)
                .enabled(editable)
            ]
            .spacing(4),
        );
        match state.authentication {
            SecretKind::Keychain => {
                body=body.push(column![text("Source password").size(style::SMALL).color(p.secondary),text_input::text_input(if state.selected.is_some(){"Leave blank to keep the saved password"}else{"Password"},state.password.as_str()).name("Source password").id(format!("{prefix}-password")).secure(true).size(style::BODY).padding([7,10]).width(Fill).style(style::input).on_input_maybe(editable.then_some(|value:String|Input::Password(value.into()))),text("Saved in the OS credential store, never in app preferences or model requests.").size(style::SMALL).color(p.secondary).wrapping(text::Wrapping::WordOrGlyph)].spacing(4));
            }
            SecretKind::Environment => {
                body = body
                    .push(field(
                        "Password environment variable",
                        &state.environment,
                        Input::Environment,
                    ))
                    .push(
                        text("Enter its CAPTUREFAB_SIGNAL_ variable name, never its value.")
                            .size(style::SMALL)
                            .color(p.secondary),
                    );
            }
            SecretKind::None => {}
        }
        let valid = state.draft_error().is_none()
            && (state.authentication != SecretKind::Keychain
                || state.selected.is_some()
                || !state.password.as_str().is_empty());
        body = body.push(
            button(text("Save profile").size(style::BODY))
                .name("Save protected source profile")
                .id(format!("{prefix}-save"))
                .padding([7, 12])
                .style(style::secondary)
                .on_press_maybe((editable && valid).then_some(Input::Save)),
        );
        if let Some(error) = state
            .draft_error()
            .filter(|_| !state.name.is_empty() && !state.endpoint.is_empty())
        {
            body = body.push(widgets::field_error(error, p));
        }
    } else if let Some(profile) = state
        .profiles
        .iter()
        .find(|profile| Some(&profile.profile) == state.selected.as_ref())
    {
        if profile.insecure {
            body = body.push(
                text("This endpoint uses an unencrypted connection.")
                    .size(style::SMALL)
                    .color(p.ink(p.warn)),
            );
        }
        body = body
            .push(
                text(crate::tools::redact::for_output_text(&profile.endpoint))
                    .size(style::BODY)
                    .wrapping(text::Wrapping::WordOrGlyph),
            )
            .push(
                text(match profile.secret {
                    SecretKind::None => "No authentication",
                    SecretKind::Keychain => "Password: OS credential store",
                    SecretKind::Environment => "Password: environment variable",
                })
                .size(style::SMALL)
                .color(p.secondary),
            );
        body = body.push(
            row![
                button(
                    text(match purpose {
                        Purpose::Approve => "Use source",
                        Purpose::Log => "Start logging",
                    })
                    .size(style::BODY)
                )
                .name(match purpose {
                    Purpose::Approve => "Use protected source in this session",
                    Purpose::Log => "Start logging protected source",
                })
                .padding([7, 12])
                .style(style::secondary)
                .on_press_maybe(editable.then_some(Input::Use)),
                button(text("Edit").size(style::SMALL))
                    .name("Edit protected source profile")
                    .padding([7, 10])
                    .style(style::plain)
                    .on_press_maybe(editable.then_some(Input::Edit)),
                button(text("Remove profile").size(style::SMALL))
                    .name("Remove protected source profile")
                    .padding([7, 10])
                    .style(style::plain)
                    .on_press_maybe(editable.then_some(Input::Delete))
            ]
            .spacing(6),
        );
    }
    if let Some(error) = &state.error {
        body = body.push(widgets::field_error(error.clone(), p));
    }
    if let Some(status) = &state.status {
        body = body.push(
            text(status)
                .size(style::SMALL)
                .color(p.secondary)
                .wrapping(text::Wrapping::WordOrGlyph),
        );
    }
    body.into()
}
