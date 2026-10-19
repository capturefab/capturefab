//! Supervised and autonomous tasks share the runtime's checked approvals,
//! budgets and journal. Constructing the editor has no external side effects.
use super::{settings::SecretDraft, sheet::SentImage, tasks_worker as worker};
use crate::{
    assistant::{
        self as ai, Settings,
        approval::{ApprovalDecision, ApprovalRequest, ApprovalScope, RunMode},
        run::{self, RunState},
    },
    gui::Level,
    tools::{self, Checked, StepId, ToolCall, context::Context},
};
pub(in crate::gui) use ai::limits::Preset;
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    sync::{
        Arc,
        atomic::Ordering,
        mpsc::{Receiver, TryRecvError},
    },
    time::{Duration, Instant},
};
#[cfg(test)]
#[path = "tasks_tests.rs"]
mod tests;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::gui) enum Mode {
    #[default]
    Plan,
    Supervised,
    Autonomous,
}
impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Plan => "Plan first",
            Self::Supervised => "Ask before each change",
            Self::Autonomous => "Run on its own",
        })
    }
}
impl fmt::Display for Preset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Short => "Short",
            Self::Standard => "Standard",
            Self::Long => "Long",
        })
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::gui) enum Tab {
    #[default]
    Activity,
    Changes,
}
#[derive(Clone, Debug)]
pub(in crate::gui) struct ActivityEntry {
    pub time_ms: u64,
    pub summary: String,
    pub level: Level,
}
#[derive(Clone, Debug)]
pub(in crate::gui) struct ChangeEntry {
    pub time_ms: u64,
    pub camera: String,
    pub feature: String,
    pub before: String,
    pub after: String,
    pub can_restore: bool,
}
#[derive(Clone, Debug)]
pub(in crate::gui) struct CrashRow {
    pub run_id: String,
    pub time_ms: u64,
}
#[derive(Clone, Debug)]
pub(in crate::gui) struct RestorePreview {
    pub calls: Vec<ToolCall>,
    pub checked: Vec<Checked>,
    pub skipped: Vec<run::RestoreSkipped>,
}
#[derive(Clone)]
pub(in crate::gui) enum Input {
    Mode(Mode),
    Goal(String),
    Camera(String, bool),
    Destination(String, bool),
    SeeImages(bool),
    Preset(Preset),
    Limits(bool),
    Limit { field: String, value: String },
    CommitLimit(String),
    Start,
    Stop,
    Pause,
    Resume,
    TakeOver(Option<String>),
    Message(String),
    SendMessage,
    Tab(Tab),
    ApprovalNote(String),
    ApprovalSecret(SecretDraft),
    Approve { id: u64, scope: ApprovalScope },
    Deny(u64),
    Restore,
    RestoreChange(usize),
    ConfirmRestore,
    CancelRestore,
    Cleanup(usize),
    ExportLog,
    ViewLog,
    CloseLog,
    NewTask,
    Open(bool),
    Settings,
    Scrolled(bool),
    ConfirmQuit,
    KeepRunning,
}
impl fmt::Debug for Input {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ApprovalSecret(_) => f.write_str("ApprovalSecret([redacted])"),
            Self::Goal(_) => f.write_str("Goal([redacted])"),
            Self::Limit { field, .. } => f
                .debug_tuple("Limit")
                .field(field)
                .field(&"[redacted]")
                .finish(),
            Self::Message(_) => f.write_str("Message([redacted])"),
            Self::ApprovalNote(_) => f.write_str("ApprovalNote([redacted])"),
            _ => f.write_str("TaskInput"),
        }
    }
}
pub(in crate::gui) enum Effect {
    None,
    ModeChanged(Mode),
}

pub(in crate::gui) struct State {
    pub mode: Mode,
    pub goal: String,
    pub cameras: BTreeSet<String>,
    pub destinations: BTreeSet<String>,
    pub allow_images: bool,
    pub limits: ai::Limits,
    pub limit_drafts: BTreeMap<String, String>,
    pub limits_open: bool,
    pub preset: Preset,
    pub settings: Settings,
    pub context: Option<Context>,
    pub snapshot: run::RunSnapshot,
    pub outcome: Option<run::RunOutcome>,
    pub approval: Option<ApprovalRequest>,
    pub approval_note: String,
    pub approval_secret: SecretDraft,
    pub latest: Option<SentImage>,
    pub latest_at_ms: Option<u64>,
    pub latest_camera: Option<String>,
    pub metrics: String,
    pub activity: Vec<ActivityEntry>,
    pub changes: Vec<ChangeEntry>,
    pub tab: Tab,
    pub message: String,
    pub error: Option<String>,
    pub provider_excerpt: Option<String>,
    pub crash: Option<CrashRow>,
    pub restore_preview: Option<RestorePreview>,
    pub synthetic: bool,
    pub scene_active: bool,
    pub now_ms: u64,
    pub log_text: Option<String>,
    pub scrolled: bool,
    services: Arc<dyn worker::Services>,
    run_services: Option<Arc<dyn worker::Services>>,
    recovery_guard: Option<worker::RecoveryGuard>,
    next_settings: Settings,
    running: Option<worker::Running>,
    job: Option<Receiver<worker::Reply>>,
    scope_initialized: bool,
    current_selection: Option<String>,
    limits_initialized: bool,
    limits_edited: bool,
    seen_crashes: BTreeSet<String>,
    takeover: Option<String>,
    ready_takeover: Option<String>,
    notice: Option<(String, Option<String>, Level)>,
    in_flight: bool,
    elapsed_anchor: Option<(Instant, u64, u64)>,
    #[cfg(test)]
    test_handle: Option<run::RunHandle>,
}
impl Default for State {
    fn default() -> Self {
        Self::with_services(Arc::new(worker::Unavailable))
    }
}
impl State {
    pub(in crate::gui) fn with_services(services: Arc<dyn worker::Services>) -> Self {
        let limits = ai::Limits::default();
        Self {
            mode: Mode::Plan,
            goal: String::new(),
            cameras: BTreeSet::new(),
            destinations: BTreeSet::new(),
            allow_images: false,
            limit_drafts: limit_fields(&limits),
            limits,
            limits_open: false,
            preset: Preset::Standard,
            settings: Settings::default(),
            context: None,
            snapshot: run::RunSnapshot::default(),
            outcome: None,
            approval: None,
            approval_note: String::new(),
            approval_secret: SecretDraft::default(),
            latest: None,
            latest_at_ms: None,
            latest_camera: None,
            metrics: String::new(),
            activity: Vec::new(),
            changes: Vec::new(),
            tab: Tab::Activity,
            message: String::new(),
            error: None,
            provider_excerpt: None,
            crash: None,
            restore_preview: None,
            synthetic: false,
            scene_active: false,
            now_ms: 0,
            log_text: None,
            scrolled: false,
            services,
            run_services: None,
            recovery_guard: None,
            next_settings: Settings::default(),
            running: None,
            job: None,
            scope_initialized: false,
            current_selection: None,
            limits_initialized: false,
            limits_edited: false,
            seen_crashes: BTreeSet::new(),
            takeover: None,
            ready_takeover: None,
            notice: None,
            in_flight: false,
            elapsed_anchor: None,
            #[cfg(test)]
            test_handle: None,
        }
    }
    pub(in crate::gui) fn configure(
        &mut self,
        settings: Settings,
        services: Arc<dyn worker::Services>,
    ) {
        if self.synthetic {
            return;
        }
        if !self.limits_initialized || !self.limits_edited && self.editable() {
            self.limits = settings.autonomy.limits.clone();
            self.limit_drafts = limit_fields(&self.limits);
            self.limits_initialized = true;
        }
        self.next_settings = settings.clone();
        if self.running.is_none() && self.outcome.is_none() && self.snapshot.run_id.is_empty() {
            self.settings = settings;
        }
        self.services = services;
    }
    pub(in crate::gui) fn set_context(&mut self, context: Context, selected: Option<String>) {
        if self.synthetic {
            return;
        }
        self.current_selection = selected.clone();
        if !self.scope_initialized
            && let Some(camera) = selected
        {
            self.cameras.insert(camera);
            self.scope_initialized = true;
        }
        self.context = Some(context);
    }
    pub(in crate::gui) fn active(&self) -> bool {
        if self.scene_active {
            return true;
        }
        #[cfg(test)]
        if let Some(h) = &self.test_handle {
            return !h.is_finished();
        }
        self.running
            .as_ref()
            .is_some_and(|r| !r.done.load(Ordering::SeqCst) && !r.control.is_finished())
    }
    pub(in crate::gui) fn busy(&self) -> bool {
        self.in_flight || self.job.is_some()
    }
    pub(in crate::gui) fn editable(&self) -> bool {
        !self.active()
            && self.outcome.is_none()
            && self.job.is_none()
            && self.snapshot.run_id.is_empty()
    }
    pub(in crate::gui) fn actor_id(&self) -> Option<String> {
        if !self.snapshot.actor_id.is_empty() {
            Some(self.snapshot.actor_id.clone())
        } else {
            None
        }
    }
    pub(in crate::gui) fn take_takeover(&mut self) -> Option<String> {
        self.ready_takeover.take()
    }
    pub(in crate::gui) fn take_notice(&mut self) -> Option<(String, Option<String>, Level)> {
        self.notice.take()
    }
    pub(in crate::gui) fn scope_contains(&self, camera: &str) -> bool {
        self.active() && self.cameras.contains(camera)
    }
    pub(in crate::gui) fn provider_label(&self) -> String {
        self.settings
            .provider
            .map(|id| id.info().name.to_owned())
            .unwrap_or_else(|| "Choose a provider".into())
    }
    pub(in crate::gui) fn model_label(&self) -> String {
        self.settings
            .selected()
            .map(|(id, s)| s.model(id).to_owned())
            .unwrap_or_default()
    }
    pub(in crate::gui) fn status_label(&self) -> String {
        match self.snapshot.state {
            RunState::Starting => "Starting",
            RunState::Running => "Working",
            RunState::Waiting => "Waiting",
            RunState::WaitingApproval => "Needs approval",
            RunState::Paused => "Paused",
            RunState::Stopping => "Stopping",
            RunState::Finished => "Done",
            RunState::NeedsUser => "Needs your help",
            RunState::Stopped => "Stopped",
            RunState::LimitReached => "Limit reached",
            RunState::Failed => "Could not finish",
        }
        .into()
    }
    pub(in crate::gui) fn can_pause(&self) -> bool {
        self.active()
            && matches!(
                self.snapshot.state,
                RunState::Running | RunState::Waiting | RunState::WaitingApproval
            )
    }
    pub(in crate::gui) fn can_resume(&self) -> bool {
        self.active() && matches!(self.snapshot.state, RunState::Paused | RunState::NeedsUser)
    }
    fn prices(&self) -> Option<&ai::usage::Prices> {
        let id = self.settings.provider?;
        self.settings
            .prices
            .get(&format!("{id}/{}", self.model_label()))
    }
    pub(in crate::gui) fn estimated_cost(&self) -> Option<(f64, f64)> {
        let p = self.prices()?;
        p.validate().ok()?;
        let max = (self.limits.max_input_tokens as f64
            * p.input.max(p.cache_write).max(p.cache_read)
            + self.limits.max_output_tokens as f64 * p.output)
            / 1e6;
        max.is_finite().then_some((0.0, max))
    }
    fn vision(&self) -> Option<ai::vision::VisionDecision> {
        let (id, s) = self.settings.selected()?;
        Some(ai::vision::resolve(
            id,
            s.model(id),
            s.vision.unwrap_or_default(),
            None,
            false,
        ))
    }
    pub(in crate::gui) fn vision_enabled(&self) -> bool {
        self.vision().is_some_and(|v| v.enabled)
    }
    pub(in crate::gui) fn vision_label(&self) -> String {
        match self.vision() {
            Some(v) if v.enabled => match v.source {
                ai::vision::VisionSource::UserOverride => "Images enabled in Settings".into(),
                _ => "Images supported by this model".into(),
            },
            Some(v) if v.mode == ai::VisionMode::Off => {
                "Images are off in provider settings".into()
            }
            Some(_) => "Measurements only · vision is unavailable or unknown".into(),
            None => "Choose a provider to check image support".into(),
        }
    }
    pub(in crate::gui) fn vision_suggestion(&self) -> Option<String> {
        let id = self.settings.provider?;
        (!self.vision_enabled())
            .then(|| {
                id.info()
                    .suggested_vision_model
                    .map(|s| format!("For camera images, try {s} and enable vision in Settings."))
            })
            .flatten()
    }
    pub(in crate::gui) fn approval_remaining(&self, _: u64) -> Duration {
        Duration::from_millis(
            self.approval
                .as_ref()
                .map(|r| r.expires_at_ms.saturating_sub(self.now_ms))
                .unwrap_or(0),
        )
    }
    pub(in crate::gui) fn valid_to_start(&self) -> bool {
        self.start_reason().is_none()
    }
    pub(in crate::gui) fn start_reason(&self) -> Option<String> {
        if !self.editable() {
            return Some("Finish the current task before starting another.".into());
        }
        if self.mode == Mode::Plan {
            return Some("Choose a camera task mode.".into());
        }
        if let Err(e) = ai::prompt::validate_prompt(&self.goal) {
            return Some(e.to_string());
        }
        if self.settings.provider.is_none() {
            return Some("Choose a provider in Settings.".into());
        }
        if let Some((id, s)) = self.settings.selected()
            && let Err(issues) = s.validate(id)
        {
            return Some(
                issues
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(" "),
            );
        }
        if self.cameras.is_empty() || self.cameras.len() > 8 {
            return Some("Choose from one to eight cameras.".into());
        }
        let Some(ctx) = &self.context else {
            return Some("Waiting for the camera session…".into());
        };
        if !ctx.session {
            return Some("Open a camera session before starting this task.".into());
        }
        if self
            .cameras
            .iter()
            .any(|id| !ctx.snapshot.cameras.iter().any(|c| &c.info.id == id))
        {
            return Some("Connect each selected camera before starting.".into());
        }
        if self
            .destinations
            .iter()
            .any(|name| !ctx.destinations.iter().any(|d| &d.name == name))
        {
            return Some("Choose available saved destinations.".into());
        }
        if let Some(error) = self.limit_drafts.keys().find_map(|k| self.limit_error(k)) {
            return Some(error);
        }
        let ceiling = ctx
            .storage
            .free_budget()
            .min(ctx.storage.free_disk.unwrap_or(u64::MAX));
        let limits = match self.parsed_limits() {
            Ok(limits) => limits,
            Err(error) => return Some(error),
        };
        ai::limits::Limiter::new(limits, self.now_ms, ceiling, self.prices().copied())
            .err()
            .map(|e| e.to_string())
    }
    fn parsed_limits(&self) -> Result<ai::Limits, String> {
        let mut value = serde_json::to_value(&self.limits).expect("limits JSON");
        for (field, raw) in &self.limit_drafts {
            let number = if field == "max_cost_usd" {
                if raw.trim().is_empty() {
                    serde_json::Value::Null
                } else {
                    let cost = raw
                        .trim()
                        .parse::<f64>()
                        .map_err(|_| "Enter a finite cost in US dollars.".to_owned())?;
                    serde_json::Number::from_f64(cost)
                        .map(serde_json::Value::Number)
                        .ok_or_else(|| "Enter a finite cost in US dollars.".to_owned())?
                }
            } else {
                let number = if field.ends_with("_ms") {
                    tools::parse_duration_field(raw)?
                } else if field == "max_bytes" {
                    crate::storage::parse_bytes(raw).map_err(|e| e.to_string())?
                } else {
                    raw.trim()
                        .parse::<u64>()
                        .map_err(|_| "Enter a whole nonnegative number.".to_owned())?
                };
                serde_json::json!(number)
            };
            value[field] = number;
        }
        let limits: ai::Limits = serde_json::from_value(value)
            .map_err(|_| "Enter a number within the supported range.".to_owned())?;
        limits.validate().map_err(|e| e.join(" "))?;
        Ok(limits)
    }
    pub(in crate::gui) fn limit_error(&self, field: &str) -> Option<String> {
        let mut state = Self {
            limits: self.limits.clone(),
            limit_drafts: limit_fields(&self.limits),
            ..Self::default()
        };
        if let Some(raw) = self.limit_drafts.get(field) {
            state.limit_drafts.insert(field.into(), raw.clone());
        }
        state.parsed_limits().err()
    }
    pub(in crate::gui) fn finish_app_quit(&mut self) {
        if let Some(r) = &self.running {
            r.control.finish_app_quit();
            r.approver.secrets.clear();
        }
        #[cfg(test)]
        if let Some(h) = &self.test_handle {
            h.finish_app_quit();
        }
        self.scene_active = false;
        self.snapshot.state = RunState::Stopped;
        self.snapshot.summary = "The assistant stopped because Capturefab quit.".into();
        self.in_flight = false;
        self.approval = None;
        self.approval_secret = SecretDraft::default();
    }
    #[cfg(test)]
    pub(in crate::gui) fn mount_run_for_test(&mut self, handle: run::RunHandle) {
        self.test_handle = Some(handle);
    }
    fn enqueue(&mut self, job: worker::Job) {
        if self.synthetic || self.job.is_some() {
            return;
        }
        match worker::job(
            self.run_services
                .clone()
                .unwrap_or_else(|| self.services.clone()),
            job,
        ) {
            Ok(r) => self.job = Some(r),
            Err(e) => self.error = Some(e),
        }
    }
    fn activity(&mut self, text: String, level: Level) {
        let mut text = tools::redact::for_output_text(&text);
        if text.len() > 4096 {
            let mut end = 4096;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
            text.push('…');
        }
        self.activity.push(ActivityEntry {
            time_ms: self.now_ms,
            summary: text,
            level,
        });
        while self.activity.len() > 256
            || self.activity.iter().map(|a| a.summary.len()).sum::<usize>() > 512 * 1024
        {
            self.activity.remove(0);
        }
    }
    fn clear_approval(&mut self) {
        self.approval = None;
        self.approval_note.clear();
        self.approval_secret = SecretDraft::default();
    }
    pub(in crate::gui) fn input(&mut self, input: Input, _now: Instant) -> Effect {
        if self.now_ms == 0 && !self.synthetic {
            self.now_ms = wall_ms();
        }
        match input {
            Input::Mode(mode) if self.editable() => {
                self.mode = mode;
                return Effect::ModeChanged(mode);
            }
            Input::Goal(value) if self.editable() => self.goal = value,
            Input::Camera(id, on) if self.editable() => {
                self.scope_initialized = true;
                if on {
                    self.cameras.insert(id);
                } else {
                    self.cameras.remove(&id);
                }
            }
            Input::Destination(name, on) if self.editable() => {
                if on {
                    self.destinations.insert(name);
                } else {
                    self.destinations.remove(&name);
                }
            }
            Input::SeeImages(on) if self.editable() => self.allow_images = on,
            Input::Preset(p) if self.editable() => {
                self.limits_edited = true;
                self.preset = p;
                self.limits = p.limits();
                self.limit_drafts = limit_fields(&self.limits);
            }
            Input::Limits(open) => self.limits_open = open,
            Input::Limit { field, value }
                if self.editable() && self.limit_drafts.contains_key(&field) =>
            {
                self.limits_edited = true;
                self.limit_drafts.insert(field, value);
                if let Ok(l) = self.parsed_limits() {
                    self.limits = l;
                }
            }
            Input::CommitLimit(field) if self.editable() => {
                if self.limit_error(&field).is_none()
                    && let Ok(l) = self.parsed_limits()
                {
                    self.limits = l;
                    self.limit_drafts = limit_fields(&self.limits);
                }
            }
            Input::Start if self.valid_to_start() && !self.synthetic => {
                self.limits = self.parsed_limits().expect("validated limits");
                let request = run::RunRequest {
                    goal: self.goal.clone(),
                    mode: if self.mode == Mode::Supervised {
                        RunMode::Supervised
                    } else {
                        RunMode::Autonomous
                    },
                    scope: run::RunScope {
                        cameras: self.cameras.iter().cloned().collect(),
                        destinations: self.destinations.iter().cloned().collect(),
                        allow_images: self.allow_images,
                        allow_recordings: self.limits.max_recordings > 0,
                        allow_schedules: self.limits.max_captures > 0,
                        effects_may_outlive_run: false,
                    },
                    limits: self.limits.clone(),
                    on_human_change: run::OnHumanChange::Pause,
                    provider: self.settings.provider,
                    model: None,
                    wait_for_answer: true,
                };
                match worker::start(self.services.clone(), self.settings.clone(), request) {
                    Ok(r) => {
                        self.running = Some(r);
                        self.run_services = Some(self.services.clone());
                        self.snapshot = run::RunSnapshot::default();
                        self.error = None;
                        self.provider_excerpt = None;
                        self.in_flight = true;
                    }
                    Err(e) => self.error = Some(e),
                }
            }
            Input::Stop => {
                if let Some(r) = &self.running {
                    r.control.stop();
                }
                #[cfg(test)]
                if let Some(h) = &self.test_handle {
                    h.stop();
                }
                if self.active() {
                    self.snapshot.state = RunState::Stopping;
                    self.snapshot.summary = "Finishing the current camera command…".into();
                }
                self.clear_approval();
            }
            Input::Pause if self.active() => {
                if let Some(r) = &self.running {
                    r.control.pause();
                }
                self.clear_approval();
            }
            Input::Resume if self.can_resume() => {
                if let Some(r) = &self.running {
                    r.control.resume();
                }
            }
            Input::TakeOver(camera) if self.active() => {
                self.takeover = camera.or_else(|| self.cameras.iter().next().cloned());
                let _ = self.input(Input::Stop, _now);
            }
            Input::Message(value) if self.active() => self.message = value,
            Input::SendMessage if self.active() => {
                if let Some(r) = &self.running {
                    match r.control.message(self.message.clone()) {
                        Ok(()) => {
                            self.activity("Message sent for the next round.".into(), Level::Done);
                            self.message.clear();
                        }
                        Err(e) => self.error = Some(e.to_string()),
                    }
                }
            }
            Input::Tab(tab) => self.tab = tab,
            Input::ApprovalNote(value) if self.approval.is_some() => self.approval_note = value,
            Input::ApprovalSecret(value)
                if self.approval.as_ref().is_some_and(|a| a.needs_secret) =>
            {
                self.approval_secret = value
            }
            Input::Approve { id, scope } => {
                let Some(req) = self
                    .approval
                    .as_ref()
                    .filter(|a| a.id == id && a.expires_at_ms > self.now_ms)
                    .cloned()
                else {
                    return Effect::None;
                };
                if scope == ApprovalScope::ForRun && !req.allow_run_scope {
                    return Effect::None;
                }
                if req.needs_secret
                    && (self.approval_secret.as_str().is_empty()
                        || self.approval_secret.as_str().len() > 4096
                        || self.approval_secret.as_str().contains(['\r', '\n', '\0']))
                {
                    self.error = Some("Enter the secret access key before approving.".into());
                    return Effect::None;
                }
                if let Some(r) = &self.running {
                    let by = self
                        .run_services
                        .as_ref()
                        .unwrap_or(&self.services)
                        .human_actor();
                    if r.approver.respond(
                        &req,
                        ApprovalDecision::Approved { scope, by },
                        self.approval_secret.clone(),
                    ) {
                        self.clear_approval();
                    }
                }
            }
            Input::Deny(id) => {
                let Some(req) = self.approval.as_ref().filter(|a| a.id == id).cloned() else {
                    return Effect::None;
                };
                let note = if self.approval_note.trim().is_empty() {
                    None
                } else {
                    if let Err(e) = ai::prompt::validate_prompt(&self.approval_note) {
                        self.error = Some(e.to_string());
                        return Effect::None;
                    }
                    Some(self.approval_note.clone())
                };
                if let Some(r) = &self.running {
                    let by = self
                        .run_services
                        .as_ref()
                        .unwrap_or(&self.services)
                        .human_actor();
                    r.approver.respond(
                        &req,
                        ApprovalDecision::Denied { note, by },
                        SecretDraft::default(),
                    );
                }
                self.clear_approval();
            }
            Input::Restore | Input::RestoreChange(_) => {
                if !self.active()
                    && let Some(outcome) = &self.outcome
                {
                    let mut record = outcome.restore.clone();
                    if let Input::RestoreChange(index) = input {
                        if let Some(change) = self.changes.get(index) {
                            record.settings.retain(|s| {
                                s.camera == change.camera && s.feature == change.feature
                            });
                        } else {
                            return Effect::None;
                        }
                    }
                    self.enqueue(worker::Job::Preview(record));
                }
            }
            Input::ConfirmRestore if !self.active() => {
                if let Some(preview) = self.restore_preview.take()
                    && preview.checked.iter().all(Checked::ready)
                    && !preview.calls.is_empty()
                {
                    let calls = preview
                        .checked
                        .iter()
                        .map(|c| (c.id, c.call.clone()))
                        .collect();
                    if let Some(guard) = self.recovery_guard.take() {
                        self.enqueue(worker::Job::Apply(calls, preview.checked, guard));
                    }
                }
            }
            Input::CancelRestore => {
                self.restore_preview = None;
                self.recovery_guard = None;
            }
            Input::Cleanup(index) if !self.active() => {
                self.cleanup(index);
            }
            Input::ViewLog => {
                if let Some(id) = self.log_id() {
                    self.enqueue(worker::Job::Read(id));
                }
            }
            Input::ExportLog => {
                if let Some(id) = self.log_id() {
                    self.enqueue(worker::Job::Export(id));
                }
            }
            Input::CloseLog => self.log_text = None,
            Input::NewTask if !self.active() && self.job.is_none() => {
                let services = self.services.clone();
                let settings = self.next_settings.clone();
                let context = self.context.clone();
                let selected = self.current_selection.clone();
                let seen = std::mem::take(&mut self.seen_crashes);
                *self = Self::with_services(services);
                self.configure(settings, self.services.clone());
                self.seen_crashes = seen;
                if let Some(ctx) = context {
                    self.set_context(ctx, selected);
                }
                return Effect::ModeChanged(Mode::Plan);
            }
            Input::Open(open) => {
                if !open {
                    self.approval_secret = SecretDraft::default();
                } else if !self.synthetic && self.crash.is_none() && !self.active() {
                    self.enqueue(worker::Job::Crash);
                }
            }
            Input::Scrolled(on) => self.scrolled = on,
            _ => {}
        }
        Effect::None
    }
    fn log_id(&self) -> Option<String> {
        self.outcome
            .as_ref()
            .map(|o| o.run_id.clone())
            .or_else(|| (!self.snapshot.run_id.is_empty()).then(|| self.snapshot.run_id.clone()))
            .or_else(|| self.crash.as_ref().map(|c| c.run_id.clone()))
    }
    fn cleanup(&mut self, index: usize) {
        let Some(outcome) = &self.outcome else { return };
        let Some(left) = outcome.leftovers.get(index).cloned() else {
            return;
        };
        self.enqueue(worker::Job::Cleanup(outcome.restore.clone(), left));
    }
    pub(in crate::gui) fn tick(&mut self, now: Instant) {
        if self.synthetic {
            return;
        }
        self.now_ms = wall_ms();
        let mut events = Vec::new();
        let mut disconnected = false;
        if let Some(r) = &self.running {
            while let Ok(request) = r.approvals.try_recv() {
                self.approval = Some(request);
                self.approval_note.clear();
                self.approval_secret = SecretDraft::default();
                self.in_flight = false;
            }
            loop {
                match r.events.try_recv() {
                    Ok(e) => events.push(e),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }
        for event in events {
            match event {
                worker::Event::State(s) => {
                    self.snapshot = *s;
                    self.elapsed_anchor = if matches!(
                        self.snapshot.state,
                        RunState::Finished
                            | RunState::Stopped
                            | RunState::LimitReached
                            | RunState::Failed
                    ) {
                        None
                    } else {
                        Some((now, self.snapshot.elapsed_ms, self.now_ms))
                    };
                    if matches!(self.snapshot.state, RunState::Starting | RunState::Running) {
                        self.in_flight = true;
                    }
                    if matches!(
                        self.snapshot.state,
                        RunState::Waiting
                            | RunState::Paused
                            | RunState::WaitingApproval
                            | RunState::NeedsUser
                            | RunState::Stopped
                            | RunState::Finished
                            | RunState::LimitReached
                            | RunState::Failed
                    ) {
                        self.in_flight = false;
                    }
                }
                worker::Event::Changes(changes) => {
                    for setting in changes {
                        if let Some(row) = self
                            .changes
                            .iter_mut()
                            .find(|c| c.camera == setting.camera && c.feature == setting.feature)
                        {
                            row.after = setting.last;
                            row.can_restore = !setting.needs_review;
                        } else {
                            self.changes.push(ChangeEntry {
                                time_ms: self.now_ms,
                                camera: setting.camera,
                                feature: setting.feature,
                                before: setting.before,
                                after: setting.last,
                                can_restore: !setting.needs_review,
                            });
                        }
                    }
                }
                worker::Event::Text(s) => {
                    self.in_flight = false;
                    self.activity(s, Level::Done);
                }
                worker::Event::Status(s) => {
                    self.in_flight = true;
                    self.activity(s, Level::Done);
                }
                worker::Event::Notice(s) => self.activity(s, Level::Warning),
                worker::Event::Image(image) => {
                    self.latest_camera = Some(image.part.label.clone());
                    self.latest_at_ms = Some(self.now_ms);
                    self.latest = Some(image);
                }
                worker::Event::Tool(call, output) => {
                    self.in_flight = false;
                    self.tool(call, output);
                }
                worker::Event::Finished(outcome) => {
                    self.finished(*outcome);
                }
                worker::Event::Complete(result) => {
                    self.in_flight = false;
                    self.clear_approval();
                    if let Err(failure) = result {
                        self.error = Some(failure.message);
                        self.provider_excerpt = failure.excerpt;
                        if self.outcome.is_none() {
                            self.snapshot.state = RunState::Failed;
                        }
                    }
                    self.running = None;
                }
            }
        }
        if disconnected && self.running.is_some() {
            self.running = None;
            self.in_flight = false;
            self.clear_approval();
            self.error = Some(
                "The assistant worker disconnected. Review the action log before trying again."
                    .into(),
            );
            self.snapshot.state = RunState::Failed;
        }
        let reply = self.job.as_ref().and_then(|r| match r.try_recv() {
            Ok(r) => Some(Ok(r)),
            Err(TryRecvError::Disconnected) => {
                Some(Err("The task review worker disconnected.".to_owned()))
            }
            Err(TryRecvError::Empty) => None,
        });
        if let Some(reply) = reply {
            self.job = None;
            match reply {
                Ok(reply) => self.reply(reply),
                Err(e) => self.error = Some(e),
            }
        }
        if self.active()
            && let Some((at, elapsed, wall)) = self.elapsed_anchor
        {
            let step = now
                .saturating_duration_since(at)
                .as_millis()
                .min(u128::from(u64::MAX)) as u64;
            self.snapshot.elapsed_ms = elapsed
                .saturating_add(step.max(self.now_ms.saturating_sub(wall)))
                .min(self.limits.max_wall_ms);
        }
        if self
            .approval
            .as_ref()
            .is_some_and(|a| a.expires_at_ms <= self.now_ms)
        {
            self.approval_secret = SecretDraft::default();
        }
        if !self.active()
            && let Some(camera) = self.takeover.take()
        {
            self.ready_takeover = Some(camera);
        }
    }
    fn finished(&mut self, outcome: run::RunOutcome) {
        self.snapshot.state = match outcome.outcome {
            run::Outcome::Finished => RunState::Finished,
            run::Outcome::NeedsUser => RunState::NeedsUser,
            run::Outcome::Stopped | run::Outcome::AppQuit => RunState::Stopped,
            run::Outcome::Limit => RunState::LimitReached,
            run::Outcome::Failed => RunState::Failed,
        };
        self.snapshot.summary = outcome.summary.clone();
        self.snapshot.totals = outcome.totals.clone();
        for setting in &outcome.restore.settings {
            if !self
                .changes
                .iter()
                .any(|c| c.camera == setting.camera && c.feature == setting.feature)
            {
                self.changes.push(ChangeEntry {
                    time_ms: self.now_ms,
                    camera: setting.camera.clone(),
                    feature: setting.feature.clone(),
                    before: setting.before.clone(),
                    after: setting.last.clone(),
                    can_restore: !setting.needs_review,
                });
            }
        }
        self.elapsed_anchor = None;
        self.outcome = Some(outcome);
        self.clear_approval();
        self.in_flight = false;
    }
    fn tool(&mut self, call: ToolCall, output: serde_json::Value) {
        let summary = output
            .get("summary")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(&call.tool);
        self.activity(summary.to_owned(), Level::Done);
        if call.tool == "inspect_frame" {
            self.metrics = metrics_summary(&output);
            self.latest_camera = call.args["camera"].as_str().map(str::to_owned);
        }
    }
    fn reply(&mut self, reply: worker::Reply) {
        match reply {
            worker::Reply::Crash(result) => match result {
                Ok(Some(row)) if self.seen_crashes.insert(row.run_id.clone()) => {
                    self.crash = Some(CrashRow {
                        run_id: row.run_id,
                        time_ms: row.last_ms.or(row.started_ms).unwrap_or(0),
                    })
                }
                Ok(_) => {}
                Err(e) => self.error = Some(tools::redact::for_output_text(&e)),
            },
            worker::Reply::Read(result) => match result {
                Ok(text) => self.log_text = Some(text),
                Err(e) => self.error = Some(tools::redact::for_output_text(&e)),
            },
            worker::Reply::Export(result) => match result {
                Ok(Some(_)) => {
                    self.notice = Some(("Assistant action log exported.".into(), None, Level::Done))
                }
                Ok(None) => {}
                Err(e) => self.error = Some(tools::redact::for_output_text(&e)),
            },
            worker::Reply::Preview(result) => match result {
                Ok((ctx, plan, guard)) => {
                    self.recovery_guard = Some(guard);
                    let mut state = tools::PlanState::new(&ctx);
                    let checked = plan
                        .calls
                        .iter()
                        .enumerate()
                        .map(|(i, c)| {
                            tools::check(&ctx, &mut state, StepId(i as u64 + 1), c, &|_, _| false)
                        })
                        .collect();
                    self.context = Some(*ctx);
                    self.restore_preview = Some(RestorePreview {
                        calls: plan.calls,
                        checked,
                        skipped: plan.skipped,
                    });
                }
                Err(e) => self.error = Some(tools::redact::for_output_text(&e)),
            },
            worker::Reply::Apply(result) => match result {
                Ok(_) => {
                    self.notice = Some(("Reviewed settings restored.".into(), None, Level::Done));
                    self.activity("Reviewed settings restored.".into(), Level::Done);
                }
                Err(e) => self.error = Some(tools::redact::for_output_text(&e)),
            },
        }
    }
}
fn wall_ms() -> u64 {
    ai::agent::Clock::now_ms(&ai::agent::SystemClock)
}
pub(in crate::gui) fn limit_fields(limits: &ai::Limits) -> BTreeMap<String, String> {
    serde_json::to_value(limits)
        .expect("limits JSON")
        .as_object()
        .expect("limits object")
        .iter()
        .map(|(key, value)| {
            let raw = if key.ends_with("_ms") {
                tools::format_duration_field(value.as_u64().unwrap_or(0))
            } else if key == "max_bytes" {
                format!("{} B", value.as_u64().unwrap_or(0))
            } else if value.is_null() {
                String::new()
            } else {
                value.to_string()
            };
            (key.clone(), raw)
        })
        .collect()
}

fn metrics_summary(output: &serde_json::Value) -> String {
    let mut parts = Vec::new();
    for (key, label) in [
        ("mean", "Brightness"),
        ("highlights", "Highlights"),
        ("shadows", "Shadows"),
    ] {
        if let Some(value) = output["exposure"][key].as_f64().filter(|v| v.is_finite()) {
            parts.push(format!("{label} {:.1}%", value * 100.0));
        }
    }
    if let Some(focus) = output["focus"]["laplacian"]
        .as_f64()
        .filter(|v| v.is_finite())
    {
        parts.push(format!("Focus {focus:.2}"));
    }
    if parts.is_empty()
        && let Some(note) = output["notes"]
            .as_array()
            .and_then(|n| n.first())
            .and_then(serde_json::Value::as_str)
    {
        parts.push(
            tools::redact::for_output_text(note)
                .chars()
                .take(512)
                .collect(),
        );
    }
    parts.join(" · ")
}
