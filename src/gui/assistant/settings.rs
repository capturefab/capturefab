//! AI provider setup. All environment, settings-file, credential-store and
//! provider work belongs to the serial worker; constructing this state is pure.
use crate::assistant::{self as ai, Effort, ProviderId, ProviderSettings, Settings, VisionMode};
use crate::gui::*;
use ai::provider::SettingsError;
use iced::futures::channel::oneshot;
use iced::widget::column;
use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    sync::atomic::AtomicU64,
};
use zeroize::Zeroizing;
#[path = "settings_worker.rs"]
mod worker;
#[cfg(test)]
pub(in crate::gui) use worker::Loaded;
pub(in crate::gui) use worker::{ConnectionFailure, Services};

const DEBOUNCE: Duration = Duration::from_millis(600);
const SECRET_HELP: &str =
    "Stored securely in the OS credential store. Never saved in app preferences.";

#[derive(Clone, Default)]
pub(in crate::gui) struct SecretDraft(Zeroizing<String>);
impl SecretDraft {
    pub(in crate::gui) fn as_str(&self) -> &str {
        &self.0
    }
}
impl From<String> for SecretDraft {
    fn from(value: String) -> Self {
        Self(Zeroizing::new(value))
    }
}
impl fmt::Debug for SecretDraft {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretDraft([redacted])")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::gui) enum Field {
    Model,
    BaseUrl,
    Endpoint,
    AccountId,
    GatewayId,
    Region,
    Profile,
    Project,
    Location,
}
impl Field {
    fn key(self) -> &'static str {
        match self {
            Self::Model => "model",
            Self::BaseUrl => "base_url",
            Self::Endpoint => "endpoint",
            Self::AccountId => "account_id",
            Self::GatewayId => "gateway_id",
            Self::Region => "region",
            Self::Profile => "profile",
            Self::Project => "project",
            Self::Location => "location",
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::Model => "Model",
            Self::BaseUrl => "Base URL",
            Self::Endpoint => "Endpoint",
            Self::AccountId => "Account ID",
            Self::GatewayId => "Gateway ID",
            Self::Region => "Region",
            Self::Profile => "Profile",
            Self::Project => "Project",
            Self::Location => "Location",
        }
    }
    fn get(self, settings: &ProviderSettings) -> &Option<String> {
        match self {
            Self::Model => &settings.model,
            Self::BaseUrl => &settings.base_url,
            Self::Endpoint => &settings.endpoint,
            Self::AccountId => &settings.account_id,
            Self::GatewayId => &settings.gateway_id,
            Self::Region => &settings.region,
            Self::Profile => &settings.profile,
            Self::Project => &settings.project,
            Self::Location => &settings.location,
        }
    }
    fn put(self, settings: &mut ProviderSettings, value: String) {
        let target = match self {
            Self::Model => &mut settings.model,
            Self::BaseUrl => &mut settings.base_url,
            Self::Endpoint => &mut settings.endpoint,
            Self::AccountId => &mut settings.account_id,
            Self::GatewayId => &mut settings.gateway_id,
            Self::Region => &mut settings.region,
            Self::Profile => &mut settings.profile,
            Self::Project => &mut settings.project,
            Self::Location => &mut settings.location,
        };
        *target = (!value.is_empty()).then_some(value);
    }
}

#[derive(Clone)]
pub(in crate::gui) enum Input {
    Open(bool),
    Provider(ProviderId),
    Field(Field, String),
    Suggestion(String),
    Vision(VisionMode),
    Effort(Effort),
    Secret(SecretDraft),
    SaveKey,
    RemoveKey,
    Test,
    ConnectionDetails(bool),
}
impl fmt::Debug for Input {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Open(value) => f.debug_tuple("Open").field(value).finish(),
            Self::Provider(id) => f.debug_tuple("Provider").field(id).finish(),
            Self::Field(field, _) => f
                .debug_tuple("Field")
                .field(field)
                .field(&"[redacted]")
                .finish(),
            Self::Suggestion(_) => f.write_str("Suggestion([redacted])"),
            Self::Vision(value) => f.debug_tuple("Vision").field(value).finish(),
            Self::Effort(value) => f.debug_tuple("Effort").field(value).finish(),
            Self::Secret(value) => f.debug_tuple("Secret").field(value).finish(),
            Self::SaveKey => f.write_str("SaveKey"),
            Self::RemoveKey => f.write_str("RemoveKey"),
            Self::Test => f.write_str("Test"),
            Self::ConnectionDetails(value) => {
                f.debug_tuple("ConnectionDetails").field(value).finish()
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::gui) struct ProviderRow {
    pub id: ProviderId,
    pub ready: bool,
}
impl fmt::Display for ProviderRow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} · {}",
            self.id,
            if self.ready { "Ready" } else { "Needs setup" }
        )
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
struct VisionChoice(VisionMode);
impl fmt::Display for VisionChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self.0 {
            VisionMode::Auto => "Auto",
            VisionMode::On => "On",
            VisionMode::Off => "Off",
        })
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
struct EffortChoice(Effort);
impl fmt::Display for EffortChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self.0 {
            Effort::Low => "Low",
            Effort::Medium => "Medium",
            Effort::High => "High",
        })
    }
}

pub(in crate::gui) struct State {
    settings: Settings,
    loaded: bool,
    loading: bool,
    visible: bool,
    open: bool,
    synthetic: bool,
    services: Arc<dyn Services>,
    worker: Option<worker::Worker>,
    generation: Arc<AtomicU64>,
    pending: usize,
    credential_pending: bool,
    test_pending: bool,
    dirty: BTreeMap<ProviderId, (ProviderSettings, Instant)>,
    /// Retained until an exact successful write is polled. Closing may pause
    /// ticks before an error reply lands, so the retry barrier cannot depend
    /// on the UI having received that reply first.
    unsaved: BTreeMap<ProviderId, ProviderSettings>,
    environment_ready: BTreeSet<ProviderId>,
    environment: BTreeMap<String, String>,
    original: BTreeMap<ProviderId, ProviderSettings>,
    status: Option<ai::CredentialStatus>,
    notice: Option<String>,
    error: Option<String>,
    connection: Option<Result<(), ConnectionFailure>>,
    details_open: bool,
    secret: SecretDraft,
    show_required: bool,
}
impl Default for State {
    fn default() -> Self {
        Self::with_services(Arc::new(worker::Native))
    }
}
impl State {
    pub(in crate::gui) fn with_services(services: Arc<dyn Services>) -> Self {
        Self {
            settings: Settings::default(),
            loaded: false,
            loading: false,
            visible: false,
            open: false,
            synthetic: false,
            services,
            worker: None,
            generation: Arc::new(AtomicU64::new(0)),
            pending: 0,
            credential_pending: false,
            test_pending: false,
            dirty: BTreeMap::new(),
            unsaved: BTreeMap::new(),
            environment_ready: BTreeSet::new(),
            environment: BTreeMap::new(),
            original: BTreeMap::new(),
            status: None,
            notice: None,
            error: None,
            connection: None,
            details_open: false,
            secret: SecretDraft::default(),
            show_required: false,
        }
    }
    pub(in crate::gui) fn ensure_loaded(&mut self) {
        if self.loaded || self.loading || self.synthetic {
            return;
        }
        match worker::start(self.services.clone(), self.generation.clone()) {
            Ok(worker) => {
                self.worker = Some(worker);
                self.loading = true;
                self.send(worker::Work::Load);
            }
            Err(error) => self.error = Some(error),
        }
    }
    #[cfg(test)]
    pub(in crate::gui) fn loaded(&self) -> bool {
        self.loaded
    }
    #[cfg(test)]
    pub(in crate::gui) fn settings(&self) -> &Settings {
        &self.settings
    }
    #[cfg(test)]
    pub(in crate::gui) fn open(&self) -> bool {
        self.section_open()
    }
    pub(in crate::gui) fn section_open(&self) -> bool {
        self.open || self.error.is_some() || !self.secret.0.is_empty()
    }
    pub(in crate::gui) fn busy(&self) -> bool {
        self.pending > 0 || self.loading || !self.dirty.is_empty()
    }
    pub(in crate::gui) fn clear_secret(&mut self) {
        self.secret = SecretDraft::default();
    }
    /// Measurement keys share the same accounts; update their nonsecret hint
    /// without loading settings or touching credentials on the UI thread.
    pub(in crate::gui) fn saved_key_changed(&mut self, id: ProviderId, present: bool) {
        self.settings.providers.entry(id).or_default().saved_key = present;
        self.original.entry(id).or_default().saved_key = present;
        self.request_status();
    }
    pub(in crate::gui) fn set_visible(&mut self, visible: bool) {
        if self.visible == visible {
            return;
        }
        self.visible = visible;
        self.generation.fetch_add(1, Ordering::SeqCst);
        if visible {
            self.ensure_loaded();
            self.request_status();
        } else {
            self.clear_secret();
        }
    }
    fn id(&self) -> ProviderId {
        self.settings.provider.unwrap_or_default()
    }
    fn selected(&self) -> ProviderSettings {
        self.settings.provider_settings(self.id())
    }
    fn send(&mut self, work: worker::Work) {
        if self.synthetic {
            return;
        }
        if self
            .worker
            .as_ref()
            .is_some_and(|worker| worker.send.send(work).is_ok())
        {
            self.pending += 1;
        } else {
            self.error =
                Some("The assistant Settings worker stopped. Reopen Settings to try again.".into());
        }
    }
    fn request_status(&mut self) {
        if !self.loaded
            || !self.visible
            || self.settings.provider.is_none()
            || !self.section_open()
            || self.credential_pending
            || self.synthetic
        {
            return;
        }
        self.credential_pending = true;
        self.send(worker::Work::Status(
            self.id(),
            self.selected(),
            self.generation.load(Ordering::SeqCst),
        ));
    }
    fn changed(&mut self, now: Instant) {
        let id = self.id();
        let settings = self.selected();
        self.connection = None;
        self.notice = None;
        self.error = None;
        self.settings.provider = Some(id);
        if settings.validate(id).is_ok() {
            self.dirty.insert(id, (settings, now));
        } else {
            self.dirty.remove(&id);
        }
    }
    fn can_edit(&self) -> bool {
        self.loaded && !self.credential_pending && !self.test_pending
    }
    pub(in crate::gui) fn input(&mut self, input: Input, now: Instant) {
        if let Input::Open(open) = input {
            let was = self.section_open();
            self.open = open;
            self.ensure_loaded();
            if open && !was {
                self.request_status();
            }
            if !open {
                self.clear_secret();
            }
            return;
        }
        if let Input::ConnectionDetails(open) = input {
            self.details_open = open;
            return;
        }
        if !self.can_edit() {
            return;
        }
        match input {
            Input::Provider(id) => {
                self.flush_dirty();
                self.settings.provider = Some(id);
                self.clear_secret();
                self.status = None;
                self.show_required = false;
                self.generation.fetch_add(1, Ordering::SeqCst);
                self.changed(now);
                self.request_status();
            }
            Input::Field(field, value) => {
                let id = self.id();
                field.put(self.settings.providers.entry(id).or_default(), value);
                self.changed(now);
            }
            Input::Suggestion(value) => {
                let id = self.id();
                self.settings.providers.entry(id).or_default().model = Some(value);
                self.changed(now);
            }
            Input::Vision(value) => {
                let id = self.id();
                self.settings.providers.entry(id).or_default().vision = Some(value);
                self.changed(now);
            }
            Input::Effort(value) => {
                let id = self.id();
                self.settings.providers.entry(id).or_default().effort = Some(value);
                self.changed(now);
            }
            Input::Secret(value) => {
                self.secret = value;
                self.notice = None;
                self.error = None;
            }
            Input::SaveKey if self.valid_secret() => {
                self.flush_dirty();
                self.notice = None;
                self.error = None;
                self.credential_pending = true;
                let key = std::mem::take(&mut self.secret);
                self.send(worker::Work::Store(self.id(), self.selected(), key));
            }
            Input::RemoveKey => {
                self.flush_dirty();
                self.clear_secret();
                self.notice = None;
                self.error = None;
                self.credential_pending = true;
                self.send(worker::Work::Remove(self.id(), self.selected()));
            }
            Input::Test => {
                self.show_required = true;
                if self.field_issues().is_empty() {
                    self.flush_dirty();
                    self.test_pending = true;
                    self.connection = None;
                    self.send(worker::Work::Test(self.id(), self.selected()));
                }
            }
            _ => {}
        }
    }
    fn valid_secret(&self) -> bool {
        !self.secret.0.is_empty()
            && self.secret.0.len() <= 4096
            && self.secret.0.bytes().all(|b| b.is_ascii_graphic())
    }
    fn flush_dirty(&mut self) {
        for (id, (settings, _)) in std::mem::take(&mut self.dirty) {
            self.save(id, settings);
        }
    }
    fn save(&mut self, id: ProviderId, settings: ProviderSettings) {
        self.unsaved.insert(id, settings.clone());
        self.send(worker::Work::Save(
            id,
            settings,
            self.settings.provider == Some(id),
        ));
    }
    pub(in crate::gui) fn flush(
        &mut self,
    ) -> impl Future<Output = Result<(), String>> + Send + 'static {
        let retry = self.unsaved.clone();
        let changing: BTreeSet<_> = self.dirty.keys().copied().collect();
        self.flush_dirty();
        for (id, settings) in retry {
            if !changing.contains(&id) {
                self.save(id, settings);
            }
        }
        let unsaved = !self.unsaved.is_empty();
        let (done, receiver) = oneshot::channel();
        let sent = self
            .worker
            .as_ref()
            .map(|worker| worker.send.send(worker::Work::Flush(done)).is_ok());
        async move {
            match sent {
                None if unsaved => Err("The assistant settings writer stopped. Reopen Settings and retry saving before quitting.".into()),
                None => Ok(()),
                Some(false) => Err("The assistant settings writer stopped.".into()),
                Some(true) => receiver
                    .await
                    .map_err(|_| "The assistant settings writer stopped.".to_owned())?,
            }
        }
    }
    pub(in crate::gui) fn tick(&mut self, now: Instant) {
        let ready: Vec<_> = self
            .dirty
            .iter()
            .filter(|(_, (_, at))| now.saturating_duration_since(*at) >= DEBOUNCE)
            .map(|(id, _)| *id)
            .collect();
        for id in ready {
            if let Some((settings, _)) = self.dirty.remove(&id) {
                self.save(id, settings);
            }
        }
        loop {
            let next = self.worker.as_ref().map(|worker| worker.receive.try_recv());
            match next {
                Some(Ok(reply)) => {
                    self.pending = self.pending.saturating_sub(1);
                    self.reply(reply);
                }
                Some(Err(TryRecvError::Disconnected)) => {
                    if self.pending > 0 {
                        self.error = Some("The assistant Settings worker stopped.".into());
                    }
                    self.loading = false;
                    self.loaded = false;
                    self.pending = 0;
                    self.credential_pending = false;
                    self.test_pending = false;
                    self.worker = None;
                    break;
                }
                _ => break,
            }
        }
    }
    fn reply(&mut self, reply: worker::Reply) {
        match reply {
            worker::Reply::Loaded(loaded) => {
                self.settings = loaded.settings;
                self.original = self.settings.providers.clone();
                self.environment_ready = loaded.environment_ready;
                self.environment = loaded.environment;
                self.error = (!loaded.warnings.is_empty()).then(|| loaded.warnings.join(" "));
                self.loaded = true;
                self.loading = false;
                self.request_status();
            }
            worker::Reply::Saved(id, settings, result) => match result {
                Ok(()) => {
                    if self.unsaved.get(&id) == Some(&settings) {
                        self.unsaved.remove(&id);
                    }
                    if self.unsaved.is_empty() {
                        self.error = None;
                    }
                }
                Err(error) => self.error = Some(error),
            },
            worker::Reply::Status(id, status) => {
                self.credential_pending = false;
                if id == self.id() {
                    self.status = status;
                }
            }
            worker::Reply::Credential(id, present, result) => {
                self.credential_pending = false;
                if let Some(present) = present {
                    self.settings.providers.entry(id).or_default().saved_key = present;
                    self.original.entry(id).or_default().saved_key = present;
                }
                if id == self.id() {
                    match result {
                        Ok(status) => {
                            self.status = Some(status);
                            self.notice=Some(if present==Some(true){"API key saved in the OS credential store."}else{"Saved API key removed. Environment variables still take precedence."}.into());
                        }
                        Err(error) => self.error = Some(error),
                    }
                }
            }
            worker::Reply::Tested(id, result) => {
                self.test_pending = false;
                if id == self.id() {
                    self.connection = Some(result);
                }
            }
        }
    }
    pub(in crate::gui) fn provider_rows(&self) -> Vec<ProviderRow> {
        ProviderId::ALL
            .into_iter()
            .map(|id| ProviderRow {
                id,
                ready: self.environment_ready.contains(&id)
                    || self.settings.provider_settings(id).saved_key,
            })
            .collect()
    }
    pub(in crate::gui) fn summary(&self) -> String {
        if !self.loaded {
            return "Choose a provider".into();
        }
        if self.settings.provider.is_none() {
            return "Not set up".into();
        }
        let id = self.id();
        let settings = self.selected();
        format!(
            "{} · {} · {}",
            id,
            settings.model(id),
            if self.environment_ready.contains(&id) || settings.saved_key {
                "Ready"
            } else {
                "Needs setup"
            }
        )
    }
    pub(in crate::gui) fn field_issues(&self) -> Vec<SettingsError> {
        let id = self.id();
        let settings = self.selected();
        match settings.effective(id, |name| self.environment.get(name).cloned()) {
            Ok(effective) => ai::provider::endpoints(id, &effective, effective.model(id))
                .err()
                .unwrap_or_default(),
            Err(issues) => issues,
        }
    }
    fn field_view<'a>(
        &'a self,
        field: Field,
        issues: &[SettingsError],
        p: &'static Palette,
    ) -> Element<'a, Input> {
        let id = self.id();
        let stored = self.settings.providers.get(&id);
        let value = stored
            .and_then(|settings| field.get(settings).as_deref())
            .unwrap_or("");
        let placeholder = if field == Field::Model {
            id.info().default_model
        } else if field == Field::BaseUrl {
            id.info().base_url
        } else {
            ""
        };
        let error = issues
            .iter()
            .find(|issue| issue.field == field.key())
            .filter(|_| !value.is_empty() || self.show_required)
            .map(|issue| issue.message.clone());
        let input = text_input(placeholder, value)
            .name(format!("Assistant {}", field.label().to_lowercase()))
            .description(format!("{} for the selected AI provider", field.label()))
            .invalid(error.clone())
            .on_input_maybe(
                self.can_edit()
                    .then_some(move |value| Input::Field(field, value)),
            )
            .padding([6, 10])
            .size(style::BODY)
            .style(if error.is_some() {
                style::input_invalid
            } else {
                style::input
            });
        let mut out = column![
            text(field.label()).size(style::SMALL).color(p.secondary),
            input
        ]
        .spacing(6);
        if let Some(error) = error {
            out = out.push(field_error(error, p));
        }
        out.into()
    }
    pub(in crate::gui) fn content(&self, p: &'static Palette, spin: usize) -> Element<'_, Input> {
        let open = self.section_open();
        let mut content = column![disclosure(
            "AI assistant",
            (!open).then(|| self.summary()),
            if open { 1.0 } else { 0.0 },
            Input::Open(!open),
            p
        )]
        .spacing(12);
        if !open {
            return content.into();
        }
        if !self.loaded {
            content = content.push(
                text(if self.loading {
                    "Loading assistant settings…"
                } else {
                    "Open Settings to load assistant providers."
                })
                .size(style::SMALL)
                .color(p.secondary),
            );
            if let Some(error) = &self.error {
                content = content.push(field_error(error, p));
            }
            return content.into();
        }
        let id = self.id();
        let settings = self.selected();
        let issues = self.field_issues();
        let ready = self.can_edit();
        let rows = self.provider_rows();
        let selected = self
            .settings
            .provider
            .and_then(|id| rows.iter().find(|row| row.id == id).cloned());
        content = content.push(
            column![
                text("Provider").size(style::SMALL).color(p.secondary),
                pick_list(rows, selected, |row| Input::Provider(row.id))
                    .name("Assistant provider")
                    .placeholder("Choose a provider…")
                    .enabled(ready)
                    .width(Fill)
                    .padding([6, 10])
                    .text_size(style::BODY)
                    .style(style::pick)
                    .menu_style(style::menu)
            ]
            .spacing(6),
        );
        if self.settings.provider.is_none() {
            return content.push(text("Choose a provider to set up the optional AI assistant. No request is sent when you open Settings.").size(style::SMALL).color(p.secondary)).into();
        }
        content = content.push(self.field_view(Field::Model, &issues, p));
        let mut suggestions = vec![];
        for model in std::iter::once(id.info().default_model)
            .chain(id.info().suggested_models.iter().copied())
            .chain(id.info().suggested_vision_model)
        {
            if !model.is_empty() && !suggestions.iter().any(|entry: &String| entry == model) {
                suggestions.push(model.to_owned());
            }
        }
        if !suggestions.is_empty() {
            content = content.push(
                pick_list(suggestions, None::<String>, Input::Suggestion)
                    .name("Suggested assistant model")
                    .placeholder("Choose a suggested model…")
                    .enabled(ready)
                    .width(Fill)
                    .padding([6, 10])
                    .text_size(style::BODY)
                    .style(style::pick)
                    .menu_style(style::menu),
            );
        }
        for field in [
            Field::BaseUrl,
            Field::Endpoint,
            Field::AccountId,
            Field::GatewayId,
            Field::Region,
            Field::Profile,
            Field::Project,
            Field::Location,
        ] {
            if id.info().settings.contains(&field.key()) {
                content = content.push(self.field_view(field, &issues, p));
            }
        }
        if let Some(original) = self.original.get(&id)
            && let Some(warning) = settings.changed_host_note(id, original)
        {
            content = content.push(text(warning).size(style::SMALL).color(p.ink(p.warn)));
        }
        for note in settings.connection_notes(id) {
            content = content.push(text(note).size(style::SMALL).color(p.ink(p.warn)));
        }
        let status = if self.credential_pending {
            "Checking credentials…".into()
        } else {
            self.status.as_ref().map(|status|{
            if status.ready{format!("Ready · {}",status.source)}else if settings.saved_key{"Check the saved key. It may be locked or no longer available.".into()}else{format!("Needs setup · {}",status.missing.join(" "))}
        }).unwrap_or_else(||"Credential status is checked only for this provider while this section is shown.".into())
        };
        content = content.push(accessibility::label(
            text(status.clone())
                .size(style::SMALL)
                .color(p.secondary)
                .into(),
            status,
            accessibility::Role::Status,
        ));
        content = content.push(
            column![
                text("API key").size(style::SMALL).color(p.secondary),
                text_input("API key", &self.secret.0)
                    .name(format!("{id} assistant API key"))
                    .description(SECRET_HELP)
                    .secure(true)
                    .on_input_maybe(ready.then_some(|value: String| Input::Secret(value.into())))
                    .on_submit_maybe((ready && self.valid_secret()).then_some(Input::SaveKey))
                    .padding([6, 10])
                    .size(style::BODY)
                    .style(style::input),
                text(SECRET_HELP).size(style::SMALL).color(p.secondary),
                row![
                    button(text("Save key").size(style::BODY))
                        .name("Save assistant API key in OS credential store")
                        .padding([6, 12])
                        .style(style::secondary)
                        .on_press_maybe((ready && self.valid_secret()).then_some(Input::SaveKey)),
                    button(text("Remove saved key").size(style::BODY))
                        .name("Remove saved assistant API key")
                        .padding([6, 12])
                        .style(style::secondary)
                        .on_press_maybe((ready && settings.saved_key).then_some(Input::RemoveKey))
                ]
                .spacing(8),
                text("OpenAI, Gemini and Claude keys are shared with prompt measurements.")
                    .size(style::SMALL)
                    .color(p.secondary)
            ]
            .spacing(6),
        );
        let vision = settings.vision.unwrap_or_default();
        let decision = ai::vision::resolve(id, settings.model(id), vision, None, false);
        let resolved = match decision.resolved {
            Some(true) => "On",
            Some(false) => "Off",
            None => "Unknown · off until support is known",
        };
        content=content.push(column![text("See camera images").size(style::SMALL).color(p.secondary),
            pick_list([VisionChoice(VisionMode::Auto),VisionChoice(VisionMode::On),VisionChoice(VisionMode::Off)],Some(VisionChoice(vision)),|value|Input::Vision(value.0))
                .name("Assistant camera images").enabled(ready).width(Fill).padding([6,10]).text_size(style::BODY).style(style::pick).menu_style(style::menu),
            text(format!("Resolved: {resolved}. Auto uses known model support; task images still need your consent.")).size(style::SMALL).color(p.secondary)].spacing(6));
        if supports_effort(id) {
            content = content.push(
                column![
                    text("Reasoning effort")
                        .size(style::SMALL)
                        .color(p.secondary),
                    pick_list(
                        [
                            EffortChoice(Effort::Low),
                            EffortChoice(Effort::Medium),
                            EffortChoice(Effort::High)
                        ],
                        Some(EffortChoice(settings.effort.unwrap_or_default())),
                        |value| Input::Effort(value.0)
                    )
                    .name("Assistant reasoning effort")
                    .enabled(ready)
                    .width(Fill)
                    .padding([6, 10])
                    .text_size(style::BODY)
                    .style(style::pick)
                    .menu_style(style::menu)
                ]
                .spacing(6),
            );
        }
        let test: Element<'_, Input> = if self.test_pending {
            row![
                icon::spinner(style::BODY, p.secondary, spin),
                text("Testing connection…").size(style::BODY)
            ]
            .spacing(8)
            .align_y(Alignment::Center)
            .into()
        } else {
            text("Test connection").size(style::BODY).into()
        };
        content = content.push(
            button(test)
                .name("Test assistant connection")
                .padding([6, 12])
                .style(move |theme, status| {
                    style::secondary(
                        theme,
                        if self.test_pending {
                            button::Status::Active
                        } else {
                            status
                        },
                    )
                })
                .on_press_maybe((ready && issues.is_empty()).then_some(Input::Test)),
        );
        content=content.push(text(format!("Sends a small test request to {id}. API usage may incur charges. No camera information or images are sent.")).size(style::SMALL).color(p.secondary));
        if !issues.is_empty() {
            content = content.push(
                text("Complete the provider fields above to test the connection.")
                    .size(style::SMALL)
                    .color(p.secondary),
            );
        }
        let connection = match &self.connection {
            Some(Ok(())) => Some(("Done · Connection test passed.".to_owned(), Level::Done)),
            Some(Err(error)) => Some((format!("Failed · {}", error.message), Level::Error)),
            None => None,
        };
        let status: Element<'_, Input> = if let Some((message, level)) = connection {
            accessibility::label(
                row![
                    level.mark(style::SMALL, p),
                    text(message.clone())
                        .size(style::SMALL)
                        .color(level.color(p))
                ]
                .spacing(6)
                .into(),
                message,
                if level == Level::Error {
                    accessibility::Role::Alert
                } else {
                    accessibility::Role::Status
                },
            )
        } else {
            text(" ").size(style::SMALL).into()
        };
        content = content.push(status);
        if let Some(Err(error)) = &self.connection
            && let Some(excerpt) = &error.excerpt
        {
            content = content.push(disclosure(
                "Connection details",
                None,
                if self.details_open { 1.0 } else { 0.0 },
                Input::ConnectionDetails(!self.details_open),
                p,
            ));
            if self.details_open {
                content = content.push(
                    text(format!("Provider said: {excerpt}"))
                        .size(style::SMALL)
                        .wrapping(text::Wrapping::WordOrGlyph)
                        .color(p.secondary),
                );
            }
        }
        if let Some(notice) = &self.notice {
            content = content.push(text(notice).size(style::SMALL).color(p.secondary));
        }
        if let Some(error) = &self.error {
            content = content.push(field_error(error, p));
        }
        content.into()
    }
    pub(in crate::gui) fn scene(error: bool) -> Self {
        let mut state = Self {
            synthetic: true,
            loaded: true,
            visible: true,
            open: true,
            ..Self::default()
        };
        let id = if error {
            ProviderId::Custom
        } else {
            ProviderId::Anthropic
        };
        state.settings.provider = Some(id);
        state.settings.providers.insert(
            id,
            ProviderSettings {
                saved_key: true,
                model: error.then(|| "bad model".into()),
                base_url: error.then(|| "http://public.example.test/v1".into()),
                ..ProviderSettings::default()
            },
        );
        state.original = state.settings.providers.clone();
        state.secret = "synthetic-key-for-layout".to_owned().into();
        state.status = Some(ai::CredentialStatus {
            ready: true,
            source: "OS credential store".into(),
            missing: vec![],
        });
        if error {
            state.connection = Some(Err(ConnectionFailure {
                message:
                    "The provider rejected the request. Check the model and provider settings."
                        .into(),
                excerpt: Some("Choose a model that supports tools.".into()),
            }));
            state.details_open = true;
            state.show_required = true;
        }
        state
    }
}

fn supports_effort(id: ProviderId) -> bool {
    matches!(
        id.protocol(),
        ai::Protocol::Responses
            | ai::Protocol::Messages
            | ai::Protocol::Generate
            | ai::Protocol::Converse
    ) || id
        .info()
        .dialect
        .is_some_and(|dialect| dialect.effort.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[derive(Default)]
    struct Stub {
        loaded: AtomicUsize,
        status: Mutex<Vec<ProviderId>>,
        writes: Mutex<Vec<(ProviderId, ProviderSettings)>>,
        fail_saves: Mutex<BTreeSet<ProviderId>>,
        fail_once: std::sync::atomic::AtomicBool,
        selected: Mutex<Option<ProviderId>>,
        keys: Mutex<Vec<(ProviderId, bool)>>,
        tests: AtomicUsize,
    }
    #[test]
    fn a_missing_worker_cannot_report_unsaved_settings_as_flushed() {
        let mut state = State::with_services(Arc::new(Stub::default()));
        state
            .unsaved
            .insert(ProviderId::Anthropic, ProviderSettings::default());
        assert!(iced::futures::executor::block_on(state.flush()).is_err());
        assert!(!state.unsaved.is_empty());
    }
    impl Services for Stub {
        fn load(&self) -> Loaded {
            self.loaded.fetch_add(1, Ordering::SeqCst);
            *self.selected.lock().unwrap() = Some(ProviderId::Anthropic);
            Loaded {
                settings: Settings {
                    provider: Some(ProviderId::Anthropic),
                    ..Settings::default()
                },
                warnings: vec![],
                environment_ready: BTreeSet::from([ProviderId::OpenAi]),
                environment: BTreeMap::new(),
            }
        }
        fn save(
            &self,
            id: ProviderId,
            settings: ProviderSettings,
            select: bool,
        ) -> Result<(), String> {
            if self.fail_once.swap(false, Ordering::SeqCst)
                || self.fail_saves.lock().unwrap().contains(&id)
            {
                return Err(format!("Could not save {id} settings."));
            }
            if select {
                *self.selected.lock().unwrap() = Some(id);
            }
            self.writes.lock().unwrap().push((id, settings));
            Ok(())
        }
        fn status(&self, id: ProviderId, _: &ProviderSettings) -> ai::CredentialStatus {
            self.status.lock().unwrap().push(id);
            ai::CredentialStatus {
                ready: true,
                source: "Test credential store".into(),
                missing: vec![],
            }
        }
        fn store(&self, id: ProviderId, _: &str) -> Result<(), String> {
            self.keys.lock().unwrap().push((id, true));
            Ok(())
        }
        fn remove(&self, id: ProviderId) -> Result<(), String> {
            self.keys.lock().unwrap().push((id, false));
            Ok(())
        }
        fn test(&self, _: ProviderId, _: ProviderSettings) -> Result<(), ConnectionFailure> {
            self.tests.fetch_add(1, Ordering::SeqCst);
            Err(ConnectionFailure {
                message: "The provider rejected the request.".into(),
                excerpt: Some("Choose another model.".into()),
            })
        }
    }
    fn settle(state: &mut State) {
        let until = Instant::now() + Duration::from_secs(3);
        while state.pending > 0 || state.loading {
            state.tick(Instant::now());
            assert!(Instant::now() < until, "Settings worker did not finish");
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    fn loaded() -> (State, Arc<Stub>) {
        let services = Arc::new(Stub::default());
        let mut state = State::with_services(services.clone());
        state.set_visible(true);
        settle(&mut state);
        (state, services)
    }
    #[test]
    fn construction_and_all_provider_readiness_do_not_read_the_credential_store() {
        let services = Arc::new(Stub::default());
        let mut state = State::with_services(services.clone());
        assert_eq!(state.provider_rows().len(), 22);
        assert_eq!(services.loaded.load(Ordering::SeqCst), 0);
        assert!(services.status.lock().unwrap().is_empty());
        state.set_visible(true);
        settle(&mut state);
        assert!(state.loaded());
        assert_eq!(services.loaded.load(Ordering::SeqCst), 1);
        assert!(
            services.status.lock().unwrap().is_empty(),
            "The closed disclosure must not prompt for credentials"
        );
        state
            .settings
            .providers
            .entry(ProviderId::Groq)
            .or_default()
            .saved_key = true;
        let rows = state.provider_rows();
        assert!(
            rows.iter()
                .find(|row| row.id == ProviderId::OpenAi)
                .unwrap()
                .ready
        );
        assert!(
            rows.iter()
                .find(|row| row.id == ProviderId::Groq)
                .unwrap()
                .ready
        );
        assert_eq!(rows.iter().filter(|row| row.ready).count(), 2);
        assert!(services.status.lock().unwrap().is_empty());
        state.input(Input::Open(true), Instant::now());
        settle(&mut state);
        assert_eq!(&*services.status.lock().unwrap(), &[ProviderId::Anthropic]);
        state.provider_rows();
        state.provider_rows();
        assert_eq!(services.status.lock().unwrap().len(), 1);
    }
    #[test]
    fn only_the_visible_selected_provider_gets_a_status_read() {
        let (mut state, services) = loaded();
        state.input(Input::Open(true), Instant::now());
        settle(&mut state);
        state.input(Input::Provider(ProviderId::Groq), Instant::now());
        settle(&mut state);
        assert_eq!(
            &*services.status.lock().unwrap(),
            &[ProviderId::Anthropic, ProviderId::Groq]
        );
        state.set_visible(false);
        state.input(Input::Provider(ProviderId::OpenAi), Instant::now());
        settle(&mut state);
        assert_eq!(services.status.lock().unwrap().len(), 2);
        state.set_visible(true);
        settle(&mut state);
        assert_eq!(
            services.status.lock().unwrap().last(),
            Some(&ProviderId::OpenAi)
        );
    }
    #[test]
    fn model_and_url_validation_errors_remain_bound_to_their_fields() {
        let mut state = State::scene(false);
        state.settings.provider = Some(ProviderId::Custom);
        state.input(
            Input::Field(Field::Model, "bad model".into()),
            Instant::now(),
        );
        state.input(
            Input::Field(Field::BaseUrl, "http://public.example.test/v1".into()),
            Instant::now(),
        );
        let issues = state.field_issues();
        assert!(
            issues
                .iter()
                .any(|issue| issue.field == "model" && issue.message.starts_with("Model:"))
        );
        assert!(
            issues
                .iter()
                .any(|issue| issue.field == "base_url" && issue.message.starts_with("Base URL:"))
        );
        state.input(
            Input::Field(Field::Model, "test-model".into()),
            Instant::now(),
        );
        state.input(
            Input::Field(Field::BaseUrl, "http://127.0.0.1:8123/v1".into()),
            Instant::now(),
        );
        assert!(state.field_issues().is_empty());
    }
    #[test]
    fn messages_and_settings_never_persist_secret_or_credential_url_edits() {
        let (mut state, services) = loaded();
        let now = Instant::now();
        let secret = "sk-test-distinct-private-credential";
        let input = Input::Secret(secret.to_owned().into());
        assert!(!format!("{input:?}").contains(secret));
        state.input(input, now);
        assert!(
            !serde_json::to_string(state.settings())
                .unwrap()
                .contains(secret)
        );
        assert!(
            !format!(
                "{:?}",
                Input::Field(Field::BaseUrl, format!("https://u:{secret}@example.test"))
            )
            .contains(secret)
        );
        state.input(Input::Provider(ProviderId::Custom), now);
        settle(&mut state);
        state.input(
            Input::Field(Field::BaseUrl, format!("https://u:{secret}@example.test")),
            now,
        );
        state.tick(now + DEBOUNCE);
        settle(&mut state);
        assert!(
            services
                .writes
                .lock()
                .unwrap()
                .iter()
                .all(|(_, settings)| !serde_json::to_string(settings).unwrap().contains(secret))
        );
        state.input(Input::Secret(secret.to_owned().into()), now);
        state.set_visible(false);
        assert!(state.secret.0.is_empty());
    }
    #[test]
    fn saves_are_debounced_and_flush_waits_for_all_provider_patches() {
        let (mut state, services) = loaded();
        let now = Instant::now();
        state.input(Input::Field(Field::Model, "first-model".into()), now);
        state.input(
            Input::Field(Field::Model, "second-model".into()),
            now + Duration::from_millis(200),
        );
        state.tick(now + DEBOUNCE);
        assert!(services.writes.lock().unwrap().is_empty());
        state.tick(now + DEBOUNCE + Duration::from_millis(200));
        settle(&mut state);
        assert_eq!(services.writes.lock().unwrap().len(), 1);
        assert_eq!(
            services.writes.lock().unwrap()[0].1.model.as_deref(),
            Some("second-model")
        );
        state.input(Input::Provider(ProviderId::Groq), now);
        settle(&mut state);
        state.input(Input::Field(Field::Model, "third-model".into()), now);
        futures::executor::block_on(state.flush()).unwrap();
        settle(&mut state);
        let writes = services.writes.lock().unwrap();
        assert_eq!(writes.last().unwrap().0, ProviderId::Groq);
        assert_eq!(
            writes.last().unwrap().1.model.as_deref(),
            Some("third-model")
        );
    }
    #[test]
    fn flush_keeps_a_failed_provider_write_until_that_provider_succeeds() {
        let services = Arc::new(Stub::default());
        services
            .fail_saves
            .lock()
            .unwrap()
            .insert(ProviderId::Anthropic);
        let worker = worker::start(services.clone(), Arc::new(AtomicU64::new(0))).unwrap();
        for id in [ProviderId::Anthropic, ProviderId::Groq] {
            worker
                .send
                .send(worker::Work::Save(id, ProviderSettings::default(), true))
                .unwrap();
        }
        let (done, receive) = oneshot::channel();
        worker.send.send(worker::Work::Flush(done)).unwrap();
        assert!(
            futures::executor::block_on(receive).unwrap().is_err(),
            "Saving another provider must not erase a failed write"
        );
        services.fail_saves.lock().unwrap().clear();
        worker
            .send
            .send(worker::Work::Save(
                ProviderId::Anthropic,
                ProviderSettings::default(),
                false,
            ))
            .unwrap();
        let (done, receive) = oneshot::channel();
        worker.send.send(worker::Work::Flush(done)).unwrap();
        futures::executor::block_on(receive).unwrap().unwrap();
        assert_eq!(*services.selected.lock().unwrap(), Some(ProviderId::Groq));
    }
    #[test]
    fn retrying_close_retries_unpolled_failed_writes_without_another_edit() {
        let (mut state, services) = loaded();
        services.fail_once.store(true, Ordering::SeqCst);
        state.input(
            Input::Field(Field::Model, "retained-model".into()),
            Instant::now(),
        );
        assert!(futures::executor::block_on(state.flush()).is_err());
        // App closing suppresses ticks. The failed reply has deliberately not
        // reached State before the close-error Retry action flushes again.
        assert!(state.pending > 0);
        futures::executor::block_on(state.flush()).unwrap();
        settle(&mut state);
        assert!(state.unsaved.is_empty());
        assert_eq!(
            services
                .writes
                .lock()
                .unwrap()
                .last()
                .unwrap()
                .1
                .model
                .as_deref(),
            Some("retained-model")
        );
        assert_eq!(
            *services.selected.lock().unwrap(),
            Some(ProviderId::Anthropic)
        );
    }
    #[test]
    fn save_remove_and_connection_test_run_through_the_injected_worker() {
        let (mut state, services) = loaded();
        let now = Instant::now();
        state.input(Input::Secret("test-key".to_owned().into()), now);
        state.input(Input::SaveKey, now);
        assert!(state.busy());
        settle(&mut state);
        assert!(state.secret.0.is_empty());
        assert!(state.selected().saved_key);
        state.input(Input::RemoveKey, now);
        settle(&mut state);
        assert!(!state.selected().saved_key);
        assert_eq!(
            &*services.keys.lock().unwrap(),
            &[
                (ProviderId::Anthropic, true),
                (ProviderId::Anthropic, false)
            ]
        );
        state.input(Input::Test, now);
        assert!(state.test_pending);
        settle(&mut state);
        assert_eq!(services.tests.load(Ordering::SeqCst), 1);
        assert!(
            matches!(state.connection,Some(Err(ref error)) if error.excerpt.as_deref()==Some("Choose another model."))
        );
    }
    #[test]
    fn stale_status_requests_do_not_open_a_keychain_after_settings_closes() {
        let services = Arc::new(Stub::default());
        let generation = Arc::new(AtomicU64::new(1));
        let worker = worker::start(services.clone(), generation).unwrap();
        worker
            .send
            .send(worker::Work::Status(
                ProviderId::Anthropic,
                ProviderSettings::default(),
                0,
            ))
            .unwrap();
        let reply = worker.receive.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(reply, worker::Reply::Status(_, None)));
        assert!(services.status.lock().unwrap().is_empty());
    }
    #[test]
    fn synthetic_settings_scenes_never_start_any_worker() {
        for error in [false, true] {
            let mut state = State::scene(error);
            state.ensure_loaded();
            state.set_visible(true);
            assert!(state.worker.is_none());
            assert!(!state.busy());
            assert!(state.open());
            assert_eq!(state.field_issues().is_empty(), !error);
        }
    }
    #[test]
    fn a_saved_key_host_change_names_the_destination_before_testing() {
        let mut state = State::scene(false);
        state.settings.provider = Some(ProviderId::Custom);
        state.settings.providers.insert(
            ProviderId::Custom,
            ProviderSettings {
                base_url: Some("https://original.example/v1".into()),
                model: Some("model".into()),
                saved_key: true,
                ..ProviderSettings::default()
            },
        );
        state.original = state.settings.providers.clone();
        state.input(
            Input::Field(Field::BaseUrl, "https://changed.example/v1".into()),
            Instant::now(),
        );
        assert_eq!(
            state
                .selected()
                .changed_host_note(ProviderId::Custom, &state.original[&ProviderId::Custom])
                .as_deref(),
            Some("Your saved key will be sent to changed.example.")
        );
    }
}
