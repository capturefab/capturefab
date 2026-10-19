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
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::gui) enum TakeoverTarget {
    Camera(String),
    Data,
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
    DataSourceDraft(String),
    AddDataSource,
    DataSource(String, bool),
    SourceProfile(crate::gui::source_setup::Input),
    UseDataset(bool),
    SeparateDataset(bool),
    AllowData(bool),
    Preset(Preset),
    Limits(bool),
    Limit { field: String, value: String },
    CommitLimit(String),
    Start,
    Stop,
    Pause,
    Resume,
    TakeOver(TakeoverTarget),
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
    RetainedDatasets,
    RevealRetained(u64),
    CopyRetained(u64),
    RemoveRetained(u64),
    ConfirmRemoveRetained(u64, u64),
    CancelRemoveRetained,
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
            Self::DataSourceDraft(_) | Self::DataSource(_, _) => {
                f.write_str("DataSource([redacted])")
            }
            Self::SourceProfile(input) => f.debug_tuple("SourceProfile").field(input).finish(),
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
    pub data_sources: BTreeSet<String>,
    pub data_source_draft: String,
    pub profile_sources: Vec<tools::context::DataSourceRef>,
    pub cached_sources: BTreeMap<String, tools::context::DataSourceRef>,
    pub source_setup: crate::gui::source_setup::State,
    pub dataset: Option<String>,
    pub separate_dataset: bool,
    pub allow_data: bool,
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
    pub retained_open: bool,
    pub retained: Vec<worker::RetainedDataset>,
    pub remove_retained: Option<(u64, u64)>,
    services: Arc<dyn worker::Services>,
    run_services: Option<Arc<dyn worker::Services>>,
    recovery_guard: Option<worker::RecoveryGuard>,
    next_settings: Settings,
    running: Option<worker::Running>,
    job: Option<Receiver<worker::Reply>>,
    scope_initialized: bool,
    current_selection: Option<String>,
    dataset_baseline: Option<String>,
    limits_initialized: bool,
    limits_edited: bool,
    seen_crashes: BTreeSet<String>,
    takeover: Option<TakeoverTarget>,
    ready_takeover: Option<TakeoverTarget>,
    notice: Option<(String, Option<String>, Level)>,
    in_flight: bool,
    elapsed_anchor: Option<(Instant, u64, u64)>,
    context_refresh_requested: bool,
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
            data_sources: BTreeSet::new(),
            data_source_draft: String::new(),
            profile_sources: Vec::new(),
            cached_sources: BTreeMap::new(),
            source_setup: crate::gui::source_setup::State::default(),
            dataset: None,
            separate_dataset: false,
            allow_data: false,
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
            retained_open: false,
            retained: Vec::new(),
            remove_retained: None,
            services,
            run_services: None,
            recovery_guard: None,
            next_settings: Settings::default(),
            running: None,
            job: None,
            scope_initialized: false,
            current_selection: None,
            dataset_baseline: None,
            limits_initialized: false,
            limits_edited: false,
            seen_crashes: BTreeSet::new(),
            takeover: None,
            ready_takeover: None,
            notice: None,
            in_flight: false,
            elapsed_anchor: None,
            context_refresh_requested: false,
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
            && !self.source_setup.pending
    }
    pub(in crate::gui) fn actor_id(&self) -> Option<String> {
        if !self.snapshot.actor_id.is_empty() {
            Some(self.snapshot.actor_id.clone())
        } else {
            None
        }
    }
    pub(in crate::gui) fn take_takeover(&mut self) -> Option<TakeoverTarget> {
        self.ready_takeover.take()
    }
    pub(super) fn takeover_target(&self) -> TakeoverTarget {
        self.cameras
            .iter()
            .next()
            .cloned()
            .map_or(TakeoverTarget::Data, TakeoverTarget::Camera)
    }
    pub(super) fn take_context_refresh(&mut self) -> bool {
        std::mem::take(&mut self.context_refresh_requested)
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
        self.active()
            && matches!(self.snapshot.state, RunState::Paused | RunState::NeedsUser)
            && self.data_scope_issue().is_none()
    }
    pub(in crate::gui) fn data_scope_issue(&self) -> Option<String> {
        if self.separate_dataset
            || !self.allow_data
            || !matches!(self.snapshot.state, RunState::Paused | RunState::NeedsUser)
        {
            return None;
        }
        let context = self.context.as_ref()?;
        let current = context
            .snapshot
            .dataset
            .as_ref()
            .and_then(|dataset| dataset.slot_id.as_ref());
        let changed = if self.dataset.is_some() {
            current != self.dataset.as_ref()
        } else {
            (!self.data_sources.is_empty() || !self.profile_sources.is_empty())
                && current != self.dataset_baseline.as_ref()
        };
        changed.then(||"The approved dataset was closed or replaced. Stop this task, then start a new task to approve the current dataset.".into())
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
    pub(in crate::gui) fn source_selected(&self, reference: &str) -> bool {
        self.profile_sources
            .iter()
            .any(|source| source.reference == reference)
            || self.data_sources.iter().any(|source| {
                source == reference
                    || tools::context::DataSourceRef::from_uri(source)
                        .is_ok_and(|source| source.reference == reference)
            })
    }
    pub(in crate::gui) fn source_draft_error(&self) -> Option<String> {
        let source = self.data_source_draft.trim();
        if source.is_empty() {
            return None;
        }
        tools::context::DataSourceRef::approved(source)
            .err()
            .map(|error| tools::redact::for_output_text(&error.to_string()))
    }
    fn task_scope(&self) -> run::RunScope {
        run::RunScope {
            cameras: self.cameras.iter().cloned().collect(),
            destinations: self.destinations.iter().cloned().collect(),
            data_sources: self
                .data_sources
                .iter()
                .cloned()
                .chain(
                    self.profile_sources
                        .iter()
                        .map(|source| source.reference.clone()),
                )
                .collect(),
            source_bindings: self.profile_sources.clone(),
            cached_source_bindings: self.cached_sources.values().cloned().collect(),
            dataset: self.dataset.clone(),
            allow_images: self.allow_images,
            allow_data: self.allow_data,
            allow_recordings: self.limits.max_recordings > 0,
            allow_schedules: self.limits.max_captures > 0,
            effects_may_outlive_run: false,
        }
    }
    fn remove_source(&mut self, source: &str) {
        let reference = tools::context::DataSourceRef::from_uri(source)
            .map(|binding| binding.reference)
            .unwrap_or_else(|_| source.into());
        self.cached_sources.remove(&reference);
        self.profile_sources
            .retain(|binding| binding.reference != reference);
        self.data_sources.retain(|selected| {
            selected != source
                && selected != &reference
                && !tools::context::DataSourceRef::from_uri(selected)
                    .is_ok_and(|binding| binding.reference == reference)
        });
    }
    fn source_effects(&mut self, effects: Vec<crate::gui::source_setup::Effect>) {
        for effect in effects {
            match effect {
                crate::gui::source_setup::Effect::Bound(binding) => {
                    self.context = None;
                    self.context_refresh_requested = true;
                    self.remove_source(&binding.reference);
                    self.profile_sources.retain(|source| {
                        source.reference != binding.reference
                            && (binding.profile_id().is_none()
                                || source.profile_id() != binding.profile_id())
                    });
                    self.profile_sources.push(binding);
                }
                crate::gui::source_setup::Effect::Invalidate(profile) => {
                    self.profile_sources
                        .retain(|source| source.profile_id() != Some(profile.as_str()));
                    let removed = self
                        .cached_sources
                        .iter()
                        .filter(|(_, source)| source.profile_id() == Some(profile.as_str()))
                        .map(|(reference, _)| reference.clone())
                        .collect::<Vec<_>>();
                    for reference in removed {
                        self.remove_source(&reference);
                    }
                }
            }
        }
    }
    pub(in crate::gui) fn start_reason(&self) -> Option<String> {
        if !self.editable() {
            return Some("Finish the current task before starting another.".into());
        }
        if self.source_setup.has_uncommitted_draft() {
            return Some(crate::gui::source_setup::UNCOMMITTED_DRAFT.into());
        }
        if self.mode == Mode::Plan {
            return Some("Choose a task mode.".into());
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
        if let Err(error) = self.task_scope().validate() {
            return Some(error.to_string());
        }
        if !self.data_source_draft.trim().is_empty() {
            return Some(self.source_draft_error().unwrap_or_else(|| {
                "Add the source to this task, or clear the source field.".into()
            }));
        }
        let Some(ctx) = &self.context else {
            return Some("Waiting for the capture session…".into());
        };
        if !ctx.session {
            return Some("Open a capture session before starting this task.".into());
        }
        if self.separate_dataset {
            if !ctx.task_data_available {
                return Some("Separate task datasets are unavailable in this session. Choose the shared dataset workflow.".into());
            }
            if !self.cameras.is_empty() || self.dataset.is_some() {
                return Some("A separate dataset currently supports sensor-only tasks. Deselect cameras and the current dataset, or turn off the separate dataset choice.".into());
            }
            if !self.allow_data {
                return Some("Allow source values and dataset results to reach the AI provider before starting this data task.".into());
            }
            if self.data_sources.is_empty() && self.profile_sources.is_empty()
                || !self.cached_sources.is_empty()
                || self
                    .data_sources
                    .iter()
                    .any(|source| source.starts_with("signal:"))
            {
                return Some("A separate dataset needs new captures. Add a source endpoint or protected binding, and remove running-source selections.".into());
            }
        }
        if let Some(error) = self.profile_sources.iter().find_map(|source| {
            source
                .validate_local()
                .and_then(|()| source.validate_in(ctx))
                .err()
        }) {
            return Some(tools::redact::for_output_text(&error.to_string()));
        }
        if self.dataset.as_ref().is_some_and(|slot| {
            ctx.snapshot
                .dataset
                .as_ref()
                .and_then(|dataset| dataset.slot_id.as_ref())
                != Some(slot)
        }) {
            return Some(
                "The selected dataset was closed or replaced. Select the current dataset again."
                    .into(),
            );
        }
        if self.data_sources.iter().any(|source| {
            source.starts_with("signal:")
                && self.cached_sources.get(source).is_none_or(|selected| {
                    !ctx.snapshot
                        .signals
                        .iter()
                        .any(|status| selected.permits_cached(status))
                })
        }) {
            return Some("An approved running source changed or disappeared. Deselect it and select the current source again before starting.".into());
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
        if self.separate_dataset && (limits.max_bytes / 2).min(limits.max_data_bytes) == 0 {
            return Some("Set a positive data byte limit for the separate task dataset.".into());
        }
        ai::limits::Limiter::new(limits, self.now_ms, ceiling, self.prices().copied())
            .err()
            .map(|e| e.to_string())
    }
    pub(in crate::gui) fn task_dataset_cap(&self) -> Option<u64> {
        self.parsed_limits()
            .ok()
            .map(|limits| (limits.max_bytes / 2).min(limits.max_data_bytes))
    }
    fn task_data_request(&self) -> Option<run::TaskDataRequest> {
        (self.separate_dataset
            && self
                .context
                .as_ref()
                .is_some_and(|ctx| ctx.task_data_available))
        .then(|| run::TaskDataRequest {
            max_bytes: self.task_dataset_cap().unwrap_or(0),
        })
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
                let raw_lower = raw.trim().to_ascii_lowercase();
                let number = if field == "max_signal_ms"
                    && matches!(raw_lower.as_str(), "0" | "0s" | "0ms")
                    || field == "max_data_bytes" && matches!(raw_lower.as_str(), "0" | "0b" | "0 b")
                {
                    0
                } else if field.ends_with("_ms") {
                    tools::parse_duration_field(raw)?
                } else if field.ends_with("_bytes") {
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
        if matches!(
            &job,
            worker::Job::Retained
                | worker::Job::CopyRetained(_)
                | worker::Job::RevealRetained(_)
                | worker::Job::RemoveRetained(_)
        ) {
            self.error = None;
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
    pub(in crate::gui) fn retained_busy(&self) -> bool {
        self.job.is_some()
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
                self.source_setup.clear_secret();
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
            Input::DataSourceDraft(value) if self.editable() => self.data_source_draft = value,
            Input::AddDataSource
                if self.editable() && !self.data_source_draft.trim().is_empty() =>
            {
                if let Some(error) = self.source_draft_error() {
                    self.error = Some(error);
                } else if let Ok(binding) =
                    tools::context::DataSourceRef::approved(self.data_source_draft.trim())
                {
                    self.remove_source(&binding.reference);
                    self.data_sources.insert(binding.uri);
                    self.data_source_draft.clear();
                    self.error = None;
                }
            }
            Input::DataSource(source, on) if self.editable() => {
                if on {
                    let Some(status) = self.context.as_ref().and_then(|context| {
                        context
                            .snapshot
                            .signals
                            .iter()
                            .find(|status| status.id == source)
                    }) else {
                        self.error =
                            Some("Choose an available running source before selecting it.".into());
                        return Effect::None;
                    };
                    let binding = tools::context::DataSourceRef::cached_source(status);
                    self.remove_source(&source);
                    self.cached_sources.insert(source.clone(), binding);
                    self.data_sources.insert(source);
                    self.error = None;
                } else {
                    self.remove_source(&source);
                }
            }
            Input::SourceProfile(input) => {
                self.source_setup.set_synthetic(self.synthetic);
                let enabled = self.editable();
                let effects = self.source_setup.input(input, enabled);
                self.source_effects(effects);
            }
            Input::UseDataset(on) if self.editable() => {
                if on {
                    self.separate_dataset = false;
                }
                self.dataset = if on {
                    self.context
                        .as_ref()
                        .and_then(|context| context.snapshot.dataset.as_ref())
                        .and_then(|dataset| dataset.slot_id.clone())
                } else {
                    None
                }
            }
            Input::SeparateDataset(on) if self.editable() => {
                self.separate_dataset = on;
                if on {
                    self.dataset = None;
                }
            }
            Input::AllowData(on) if self.editable() => self.allow_data = on,
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
                self.dataset_baseline = self
                    .context
                    .as_ref()
                    .and_then(|context| context.snapshot.dataset.as_ref())
                    .and_then(|dataset| dataset.slot_id.clone());
                let request = run::RunRequest {
                    goal: self.goal.clone(),
                    mode: if self.mode == Mode::Supervised {
                        RunMode::Supervised
                    } else {
                        RunMode::Autonomous
                    },
                    scope: self.task_scope(),
                    limits: self.limits.clone(),
                    on_human_change: run::OnHumanChange::Pause,
                    provider: self.settings.provider,
                    model: None,
                    wait_for_answer: true,
                    task_data: self.task_data_request(),
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
                    self.snapshot.summary =
                        "Finishing the current capture command and flushing owned data…".into();
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
            Input::TakeOver(target) if self.active() => {
                self.takeover = Some(target);
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
            Input::RetainedDatasets if !self.active() && self.job.is_none() => {
                self.retained.clear();
                self.remove_retained = None;
                self.retained_open = true;
                self.enqueue(worker::Job::Retained);
            }
            Input::CopyRetained(key) if !self.active() && self.job.is_none() => {
                if let Some(row) = self
                    .retained
                    .iter()
                    .find(|row| row.key == key && row.copyable())
                    .cloned()
                {
                    self.enqueue(worker::Job::CopyRetained(row));
                }
            }
            Input::RevealRetained(key) if self.job.is_none() => {
                if let Some(row) = self.retained.iter().find(|row| row.key == key).cloned() {
                    self.enqueue(worker::Job::RevealRetained(row));
                }
            }
            Input::RemoveRetained(key) if !self.active() && self.job.is_none() => {
                if self
                    .retained
                    .iter()
                    .any(|row| row.key == key && row.removable())
                {
                    self.error = None;
                    self.remove_retained = Some((key, worker::retained_key()));
                }
            }
            Input::ConfirmRemoveRetained(key, review) if !self.active() && self.job.is_none() => {
                if self.remove_retained == Some((key, review))
                    && let Some(row) = self.retained.iter().find(|row| row.key == key).cloned()
                    && row.removable()
                {
                    self.remove_retained = None;
                    self.enqueue(worker::Job::RemoveRetained(row));
                }
            }
            Input::CancelRemoveRetained => self.remove_retained = None,
            Input::ExportLog => {
                if let Some(id) = self.log_id() {
                    self.enqueue(worker::Job::Export(id));
                }
            }
            Input::CloseLog => self.log_text = None,
            Input::NewTask if !self.active() && self.job.is_none() => {
                self.source_setup.clear_secret();
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
                    self.source_setup.clear_secret();
                    self.approval_secret = SecretDraft::default();
                    self.remove_retained = None;
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
        let effects = self.source_setup.tick();
        self.source_effects(effects);
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
                        if self.outcome.is_none()
                            && self
                                .running
                                .as_ref()
                                .is_some_and(|r| r.control.cancel().load(Ordering::SeqCst))
                        {
                            self.snapshot.state = RunState::Stopped;
                            self.snapshot.summary = "The assistant task was stopped.".into();
                            self.error = None;
                            self.provider_excerpt = None;
                        } else {
                            self.error = Some(failure.message);
                            self.provider_excerpt = failure.excerpt;
                            if self.outcome.is_none() {
                                self.snapshot.state = RunState::Failed;
                            }
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
            && let Some(target) = self.takeover.take()
        {
            self.ready_takeover = Some(target);
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
        if call.tool == "open_dataset"
            && let Some(slot) = output["results"].as_array().and_then(|results| {
                results
                    .iter()
                    .rev()
                    .find_map(|result| result["slot_id"].as_str())
            })
        {
            self.dataset = Some(slot.into());
        }
        if call.tool == "close_dataset" {
            self.dataset = None;
            self.dataset_baseline = None;
        }
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
            worker::Reply::Copied(result) => match result {
                Ok(Some(path)) => {
                    self.notice = Some((
                        format!(
                            "Dataset copied to {}. Open task.duckdb in that folder; keep its WAL alongside it for recovery.",
                            path.display()
                        ),
                        None,
                        Level::Done,
                    ))
                }
                Ok(None) => {}
                Err(error) => self.error = Some(error),
            },
            worker::Reply::Retained(result) => match result {
                Ok(rows) => self.retained = rows,
                Err(error) => self.error = Some(error),
            },
            worker::Reply::Revealed(result) => {
                if let Err(error) = result {
                    self.error = Some(error);
                }
            }
            worker::Reply::Removed(key, result)
                if self.retained.iter().any(|row| row.key == key) =>
            {
                match result {
                    Ok(receipt) => {
                        self.retained.retain(|row| row.key != key);
                        if let Some(receipt) = receipt
                            && let Some(outcome) = &mut self.outcome
                            && outcome.task_data.as_ref().is_some_and(|current| {
                                current.id == receipt.id
                                    && current.catalog.identity == receipt.catalog.identity
                            })
                        {
                            outcome.task_data = Some(*receipt);
                        }
                        self.notice = Some((
                            "Dataset removed and its storage reservation released.".into(),
                            None,
                            Level::Done,
                        ));
                    }
                    Err(error) => self.error = Some(error),
                }
            }
            worker::Reply::Removed(..) => {}
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
            } else if key.ends_with("_bytes") {
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
