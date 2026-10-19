//! The quick-command model. Editing checks are pure; a bounded set of named
//! workers gathers context, asks a model, runs a reviewed plan and exports it.
use super::{
    drafts::Drafts,
    runner::{self, Services},
    settings::SecretDraft,
    sheet::SentImage,
};
use crate::{
    assistant::{
        self as ai, Settings, Usage,
        agent::{Plan, PlanRequest},
        conversation::Conversation,
    },
    gui::*,
    tools::{self, Checked, StepId, ToolCall, context::Context, exec::ProgressState},
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::atomic::AtomicBool,
};

const CONTEXT_INTERVAL: Duration = Duration::from_secs(5);
const MAX_TRANSCRIPT: usize = 256;
const MAX_TEXT_BYTES: usize = 512 * 1024;
const MAX_IMAGE_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone)]
pub(in crate::gui) enum Input {
    Open(bool),
    Scrolled(bool),
    Prompt(String),
    Send,
    Retry,
    Suggestion(String),
    Stop,
    NewConversation,
    Settings,
    Run(u64),
    ExportPlan,
    SeeImages(bool),
    Field {
        step: StepId,
        key: String,
        value: String,
    },
    Commit {
        step: StepId,
        key: String,
    },
    Secret {
        step: StepId,
        key: String,
        value: SecretDraft,
    },
    Choice {
        step: StepId,
        key: String,
        value: String,
    },
    Bool {
        step: StepId,
        key: String,
        value: bool,
    },
    ScheduleEnd {
        step: StepId,
        kind: String,
    },
    Remove(StepId),
    FocusStep(StepId),
    NewDestination,
    TestS3(StepId),
    Details(StepId, bool),
    Options(StepId, bool),
    Copy(String),
    TaskMode(super::tasks::Mode),
    Task(super::tasks::Input),
}
impl fmt::Debug for Input {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Open(value) => f.debug_tuple("Open").field(value).finish(),
            Self::Scrolled(value) => f.debug_tuple("Scrolled").field(value).finish(),
            Self::Prompt(_) => f.write_str("Prompt([redacted])"),
            Self::Send => f.write_str("Send"),
            Self::Retry => f.write_str("Retry"),
            Self::Suggestion(_) => f.write_str("Suggestion([redacted])"),
            Self::Stop => f.write_str("Stop"),
            Self::NewConversation => f.write_str("NewConversation"),
            Self::Settings => f.write_str("Settings"),
            Self::Run(revision) => f.debug_tuple("Run").field(revision).finish(),
            Self::ExportPlan => f.write_str("ExportPlan"),
            Self::SeeImages(value) => f.debug_tuple("SeeImages").field(value).finish(),
            Self::Field { step, .. } => f
                .debug_tuple("Field")
                .field(step)
                .field(&"[redacted]")
                .finish(),
            Self::Commit { step, .. } => f.debug_tuple("Commit").field(step).finish(),
            Self::Secret { step, .. } => f
                .debug_tuple("Secret")
                .field(step)
                .field(&"[redacted]")
                .finish(),
            Self::Choice { step, .. } => f
                .debug_tuple("Choice")
                .field(step)
                .field(&"[redacted]")
                .finish(),
            Self::Bool { step, .. } => f.debug_tuple("Bool").field(step).finish(),
            Self::ScheduleEnd { step, .. } => f.debug_tuple("ScheduleEnd").field(step).finish(),
            Self::Remove(step) => f.debug_tuple("Remove").field(step).finish(),
            Self::FocusStep(step) => f.debug_tuple("FocusStep").field(step).finish(),
            Self::NewDestination => f.write_str("NewDestination"),
            Self::TestS3(step) => f.debug_tuple("TestS3").field(step).finish(),
            Self::Details(step, value) => {
                f.debug_tuple("Details").field(step).field(value).finish()
            }
            Self::Options(step, value) => {
                f.debug_tuple("Options").field(step).field(value).finish()
            }
            Self::Copy(_) => f.write_str("Copy([redacted])"),
            Self::TaskMode(mode) => f.debug_tuple("TaskMode").field(mode).finish(),
            Self::Task(input) => f.debug_tuple("Task").field(input).finish(),
        }
    }
}
pub(in crate::gui) enum Effect {
    None,
    FocusPrompt,
    FocusStep(StepId),
    Settings,
    NewDestination,
    Copy(String),
}

#[derive(Clone)]
pub(in crate::gui) enum TranscriptEntry {
    User(String),
    Assistant(String),
    Tool {
        title: String,
        summary: String,
        images: Vec<SentImage>,
    },
    Outcome {
        summary: String,
        level: Level,
    },
    Notice(String),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::gui) enum StepPhase {
    Running,
    Done,
    Failed,
}
#[derive(Clone, Debug)]
pub(in crate::gui) struct StepStatus {
    pub state: StepPhase,
    pub message: String,
}

struct PendingTurn {
    generation: u64,
    cancel: Arc<AtomicBool>,
    receive: Receiver<runner::TurnEvent>,
    before: Plan,
    usage_before: Usage,
    cancelled: bool,
}
struct PendingApply {
    cancel: Arc<AtomicBool>,
    receive: Receiver<runner::ApplyEvent>,
    calls: Vec<(StepId, ToolCall)>,
}
struct PendingS3 {
    step: StepId,
    call: ToolCall,
    receive: Receiver<Result<String, String>>,
}

pub(in crate::gui) struct State {
    pub open: bool,
    pub scrolled: bool,
    pub prompt: String,
    pub plan: Plan,
    pub context: Option<Context>,
    pub transcript: Vec<TranscriptEntry>,
    pub status: String,
    pub error: Option<String>,
    pub provider_excerpt: Option<String>,
    pub exported_plan: Option<PathBuf>,
    pub allow_images: bool,
    pub generation: u64,
    pub usage: Usage,
    pub step_outcomes: BTreeMap<StepId, StepStatus>,
    pub drafts: Drafts,
    pub details_open: BTreeSet<StepId>,
    pub options_open: BTreeSet<StepId>,
    pub settings: Settings,
    pub synthetic: bool,
    pub scene_thinking: bool,
    pub scene_running: bool,
    services: Arc<dyn Services>,
    conversation: Option<Conversation>,
    detected_environment: Vec<String>,
    turn: Option<PendingTurn>,
    applying: Option<PendingApply>,
    gathering: Option<Receiver<Result<Context, runner::Failure>>>,
    exporting: Option<Receiver<Result<Option<PathBuf>, String>>>,
    testing_s3: Option<PendingS3>,
    refreshed: Option<Instant>,
    last_prompt: Option<String>,
    remaining: bool,
    notice: Option<(String, Option<String>, Level)>,
}
impl Default for State {
    fn default() -> Self {
        Self::with_services(Arc::new(runner::Unavailable))
    }
}
impl State {
    pub(in crate::gui) fn with_services(services: Arc<dyn Services>) -> Self {
        Self {
            open: false,
            scrolled: false,
            prompt: String::new(),
            plan: Plan::default(),
            context: None,
            transcript: vec![],
            status: String::new(),
            error: None,
            provider_excerpt: None,
            exported_plan: None,
            allow_images: false,
            generation: 0,
            usage: Usage::default(),
            step_outcomes: BTreeMap::new(),
            drafts: Drafts::default(),
            details_open: BTreeSet::new(),
            options_open: BTreeSet::new(),
            settings: Settings::default(),
            synthetic: false,
            scene_thinking: false,
            scene_running: false,
            services,
            conversation: None,
            detected_environment: vec![],
            turn: None,
            applying: None,
            gathering: None,
            exporting: None,
            testing_s3: None,
            refreshed: None,
            last_prompt: None,
            remaining: false,
            notice: None,
        }
    }
    pub(in crate::gui) fn configure(&mut self, settings: Settings, services: Arc<dyn Services>) {
        if self.synthetic {
            return;
        }
        self.settings = settings;
        self.services = services;
    }
    pub(in crate::gui) fn set_detected_environment(&mut self, names: Vec<String>) {
        self.detected_environment = names;
    }
    pub(in crate::gui) fn detected_environment(&self) -> Vec<String> {
        self.detected_environment.clone()
    }
    pub(in crate::gui) fn synthetic(&self) -> bool {
        self.synthetic
    }
    pub(in crate::gui) fn thinking(&self) -> bool {
        self.scene_thinking || self.turn.is_some()
    }
    pub(in crate::gui) fn running(&self) -> bool {
        self.scene_running || self.applying.is_some()
    }
    pub(in crate::gui) fn busy(&self) -> bool {
        self.thinking()
            || self.running()
            || self.gathering.is_some()
            || self.exporting.is_some()
            || self.testing_s3.is_some()
    }
    pub(in crate::gui) fn editable(&self) -> bool {
        !self.thinking() && !self.running() && self.testing_s3.is_none()
    }
    pub(in crate::gui) fn has_provider(&self) -> bool {
        self.settings.provider.is_some()
    }
    pub(in crate::gui) fn provider_label(&self) -> String {
        self.settings
            .provider
            .map_or_else(|| "Set up a provider".into(), |id| id.info().name.into())
    }
    pub(in crate::gui) fn model_label(&self) -> String {
        self.settings
            .selected()
            .map_or_else(String::new, |(id, settings)| settings.model(id).to_owned())
    }
    pub(in crate::gui) fn run_label(&self) -> String {
        if self.remaining {
            "Run remaining steps"
        } else {
            "Run plan"
        }
        .into()
    }
    pub(in crate::gui) fn step_label(&self, id: StepId) -> Option<&str> {
        self.step_outcomes
            .get(&id)
            .map(|status| match status.state {
                StepPhase::Running => "Running",
                StepPhase::Done => "Done",
                StepPhase::Failed => "Failed",
            })
    }
    pub(in crate::gui) fn step_error(&self, id: StepId) -> Option<&str> {
        self.step_outcomes
            .get(&id)
            .filter(|status| status.state == StepPhase::Failed)
            .map(|status| status.message.as_str())
    }
    pub(in crate::gui) fn announcement(&self) -> String {
        let count = self.plan.steps.len();
        let attention = self
            .plan
            .steps
            .iter()
            .filter(|step| step.checked.as_ref().is_none_or(|checked| !checked.ready()))
            .count();
        format!(
            "Plan has {count} {}; {attention} {} attention.",
            if count == 1 { "step" } else { "steps" },
            if attention == 1 { "needs" } else { "need" }
        )
    }
    pub(in crate::gui) fn blocking_step(&self) -> Option<(StepId, String)> {
        self.plan
            .steps
            .iter()
            .enumerate()
            .find_map(|(index, step)| {
                if self.context.is_none() {
                    return Some((
                        step.id,
                        format!(
                            "Step {} needs attention: wait for the session details.",
                            index + 1
                        ),
                    ));
                }
                match &step.checked {
                    None => Some((
                        step.id,
                        format!(
                            "Step {} needs attention: wait for the plan check.",
                            index + 1
                        ),
                    )),
                    Some(checked) => {
                        checked
                            .issues
                            .iter()
                            .find(|issue| issue.is_error())
                            .map(|issue| {
                                (
                                    step.id,
                                    format!(
                                        "Step {} needs attention: {}",
                                        index + 1,
                                        issue.message
                                    ),
                                )
                            })
                    }
                }
            })
    }
    pub(in crate::gui) fn ready_to_run(&self) -> bool {
        !self.thinking()
            && !self.running()
            && self.testing_s3.is_none()
            && self.context.is_some()
            && !self.plan.steps.is_empty()
            && self
                .plan
                .steps
                .iter()
                .all(|step| step.checked.as_ref().is_some_and(Checked::ready))
    }
    pub(in crate::gui) fn take_notice(&mut self) -> Option<(String, Option<String>, Level)> {
        self.notice.take()
    }
    pub(in crate::gui) fn clear_secrets(&mut self) {
        let had_secret = self.plan.steps.iter().any(|step| {
            tools::spec(&step.call.tool).is_some_and(|spec| {
                spec.fields.iter().any(|field| {
                    field.kind == tools::FieldKind::Secret
                        && !self.drafts.secret_text(step.id, field.key).is_empty()
                })
            })
        });
        self.drafts.clear_secrets();
        if had_secret {
            self.plan.revision = self.plan.revision.saturating_add(1);
        }
        self.recheck();
    }
    fn set_error(&mut self, message: impl Into<String>) {
        self.error = Some(tools::redact::for_output_text(&message.into()));
        self.provider_excerpt = None;
    }
    fn changed_refusal(&mut self) {
        self.set_error("The plan changed since you reviewed it. Check it and run again.");
        self.status = self.error.clone().unwrap_or_default();
    }
    fn refresh(&mut self, now: Instant) {
        if self.synthetic || self.gathering.is_some() {
            return;
        }
        self.refreshed = Some(now);
        match runner::gather(self.services.clone()) {
            Ok(receive) => self.gathering = Some(receive),
            Err(error) => self.set_error(error),
        }
    }
    fn recheck(&mut self) {
        let Some(ctx) = &self.context else {
            return;
        };
        let checked = self.plan.steps.iter().any(|step| step.checked.is_some());
        let before = approval_view(&self.plan);
        self.drafts
            .recheck(&mut self.plan, ctx, self.services.actor_id());
        if checked && before != approval_view(&self.plan) {
            self.plan.revision = self.plan.revision.saturating_add(1);
        }
    }
    fn append(&mut self, entry: TranscriptEntry) {
        self.transcript.push(entry);
        while self.transcript.len() > MAX_TRANSCRIPT
            || transcript_bytes(&self.transcript) > MAX_TEXT_BYTES
        {
            self.transcript.remove(0);
        }
        let mut total = image_bytes(&self.transcript);
        for entry in &mut self.transcript {
            if total <= MAX_IMAGE_BYTES {
                break;
            }
            if let TranscriptEntry::Tool { images, .. } = entry {
                while total > MAX_IMAGE_BYTES && !images.is_empty() {
                    let removed = images.remove(0);
                    total = total.saturating_sub(image_footprint(&removed));
                }
            }
        }
    }
    pub(in crate::gui) fn input(&mut self, input: Input, now: Instant) -> Effect {
        match input {
            Input::Open(open) => {
                self.open = open;
                if open {
                    self.scrolled = false;
                    self.refresh(now);
                    return Effect::FocusPrompt;
                }
                self.clear_secrets();
                return Effect::None;
            }
            Input::Scrolled(scrolled) => {
                self.scrolled = scrolled;
                return Effect::None;
            }
            Input::Stop => {
                self.stop();
                return Effect::None;
            }
            Input::Details(id, open) => {
                if open {
                    self.details_open.insert(id);
                } else {
                    self.details_open.remove(&id);
                }
                return Effect::None;
            }
            Input::Options(id, open) => {
                if open {
                    self.options_open.insert(id);
                } else {
                    self.options_open.remove(&id);
                }
                return Effect::None;
            }
            Input::Copy(text) => return Effect::Copy(tools::redact::for_output_text(&text)),
            Input::Settings => {
                self.open = false;
                self.clear_secrets();
                return Effect::Settings;
            }
            Input::NewDestination => {
                self.open = false;
                self.clear_secrets();
                return Effect::NewDestination;
            }
            Input::FocusStep(id) => return Effect::FocusStep(id),
            _ => {}
        }
        if !self.editable() {
            return Effect::None;
        }
        match input {
            Input::Prompt(value) => self.prompt = value,
            Input::Suggestion(value) => self.prompt = value,
            Input::SeeImages(value) => self.allow_images = value,
            Input::Send => self.send(now),
            Input::Retry => {
                if let Some(prompt) = self.last_prompt.clone() {
                    self.prompt = prompt;
                    self.send(now);
                }
            }
            Input::NewConversation => {
                self.plan = Plan::default();
                self.conversation = None;
                self.drafts = Drafts::default();
                self.transcript.clear();
                self.prompt.clear();
                self.status.clear();
                self.error = None;
                self.provider_excerpt = None;
                self.step_outcomes.clear();
                self.details_open.clear();
                self.options_open.clear();
                self.exported_plan = None;
                self.last_prompt = None;
                self.remaining = false;
                self.usage = Usage::default();
                self.allow_images = false;
                return Effect::FocusPrompt;
            }
            Input::Field { step, key, value } => {
                let _ = self.drafts.edit(&mut self.plan, step, &key, value);
                self.step_outcomes.remove(&step);
                self.recheck();
            }
            Input::Commit { step, key } => {
                let _ = self.drafts.commit(&mut self.plan, step, &key);
                self.recheck();
            }
            Input::Secret { step, key, value } => {
                let Some(item) = self.plan.steps.iter_mut().find(|item| item.id == step) else {
                    return Effect::None;
                };
                if !tools::spec(&item.call.tool).is_some_and(|spec| {
                    spec.fields
                        .iter()
                        .any(|field| field.key == key && field.kind == tools::FieldKind::Secret)
                }) {
                    return Effect::None;
                }
                item.user_edited = true;
                item.changed_by_assistant = false;
                self.plan.revision = self.plan.revision.saturating_add(1);
                self.drafts.secret(step, &key, value);
                self.recheck();
            }
            Input::Choice { step, key, value } => {
                let _ = self
                    .drafts
                    .set_value(&mut self.plan, step, &key, Value::String(value));
                self.step_outcomes.remove(&step);
                self.recheck();
            }
            Input::Bool { step, key, value } => {
                let _ = self
                    .drafts
                    .set_value(&mut self.plan, step, &key, Value::Bool(value));
                self.step_outcomes.remove(&step);
                self.recheck();
            }
            Input::ScheduleEnd { step, kind } => self.schedule_end(step, &kind),
            Input::Remove(id) => {
                let before = self.plan.clone();
                let _ = self.plan.remove(id);
                self.drafts.reconcile(&before, &self.plan);
                self.step_outcomes.remove(&id);
                self.recheck();
            }
            Input::Run(revision) => self.run(revision),
            Input::ExportPlan => self.export(),
            Input::TestS3(id) => self.test_s3(id),
            _ => {}
        }
        Effect::None
    }
    fn schedule_end(&mut self, id: StepId, kind: &str) {
        if !["duration", "count", "until"].contains(&kind) {
            return;
        }
        let Some(step) = self.plan.steps.iter().find(|step| step.id == id) else {
            return;
        };
        if step.call.tool != "schedule_captures" {
            return;
        }
        let mut call = step.call.clone();
        let Some(args) = call.args.as_object_mut() else {
            return;
        };
        for key in ["duration", "count", "until"] {
            if key != kind {
                args.insert(key.into(), Value::Null);
            }
        }
        if args.get(kind).is_none_or(Value::is_null) {
            let value = match kind {
                "duration" => json!("1m"),
                "count" => json!(1),
                _ => self
                    .context
                    .as_ref()
                    .map(|ctx| json!(ctx.zone.rfc3339(ctx.now_ms.saturating_add(3_600_000))))
                    .unwrap_or(Value::Null),
            };
            args.insert(kind.into(), value);
        }
        let before = self.plan.clone();
        let user_camera = step.user_entered_camera;
        let _ = self.plan.edit(id, call, user_camera);
        self.drafts.reconcile(&before, &self.plan);
        self.recheck();
    }
    fn send(&mut self, now: Instant) {
        if self.synthetic {
            return;
        }
        if let Err(error) = ai::prompt::validate_prompt(&self.prompt) {
            self.set_error(error.to_string());
            return;
        }
        if !self.has_provider() {
            self.set_error("Choose an AI provider in Settings before sending a request.");
            return;
        }
        self.recheck();
        self.generation = self.generation.saturating_add(1);
        self.error = None;
        self.provider_excerpt = None;
        let prompt = std::mem::take(&mut self.prompt);
        self.last_prompt = Some(prompt.clone());
        let request = PlanRequest {
            prompt: prompt.clone(),
            provider: None,
            model: None,
            plan: self.plan.clone(),
            conversation: self.conversation.clone(),
            allow_images: self.allow_images,
        };
        let cancel = Arc::new(AtomicBool::new(false));
        match runner::turn(
            self.services.clone(),
            self.settings.clone(),
            request,
            self.drafts.snapshot(),
            cancel.clone(),
        ) {
            Ok(receive) => {
                self.append(TranscriptEntry::User(prompt));
                self.status = format!("Asking {}…", self.provider_label());
                self.turn = Some(PendingTurn {
                    generation: self.generation,
                    cancel,
                    receive,
                    before: self.plan.clone(),
                    usage_before: self.usage,
                    cancelled: false,
                });
            }
            Err(error) => {
                self.prompt = prompt;
                self.set_error(error);
            }
        }
        if self.context.is_none() {
            self.refresh(now);
        }
    }
    fn stop(&mut self) {
        if let Some(turn) = &mut self.turn
            && !turn.cancelled
        {
            turn.cancelled = true;
            turn.cancel.store(true, Ordering::SeqCst);
            self.generation = self.generation.saturating_add(1);
            let before = self.plan.clone();
            self.plan.restore_from(&turn.before);
            self.drafts.reconcile(&before, &self.plan);
            self.conversation = None;
            self.status = "Stopping…".into();
            self.recheck();
        }
        if let Some(applying) = &self.applying {
            applying.cancel.store(true, Ordering::SeqCst);
            self.status = "Stopping…".into();
        }
    }
    fn run(&mut self, revision: u64) {
        if self.synthetic {
            return;
        }
        if revision != self.plan.revision {
            self.changed_refusal();
            return;
        }
        if !self.ready_to_run() {
            if let Some((_, reason)) = self.blocking_step() {
                self.set_error(reason);
            }
            return;
        }
        let approved: Vec<_> = self
            .plan
            .steps
            .iter()
            .filter_map(|step| step.checked.clone())
            .collect();
        let calls = self.plan.calls();
        let cancel = Arc::new(AtomicBool::new(false));
        match runner::apply(
            self.services.clone(),
            calls.clone(),
            approved,
            self.drafts.snapshot(),
            cancel.clone(),
        ) {
            Ok(receive) => {
                self.error = None;
                self.provider_excerpt = None;
                self.status = "Checking the reviewed plan…".into();
                self.step_outcomes.clear();
                self.applying = Some(PendingApply {
                    cancel,
                    receive,
                    calls,
                });
            }
            Err(error) => self.set_error(error),
        }
    }
    fn export(&mut self) {
        if self.synthetic || self.exporting.is_some() {
            return;
        }
        let document = self.plan.document();
        if let Err(error) = tools::plan::to_json(&document) {
            self.set_error(error.to_string());
            return;
        }
        match runner::export(self.services.clone(), document) {
            Ok(receive) => self.exporting = Some(receive),
            Err(error) => self.set_error(error),
        }
    }
    fn test_s3(&mut self, id: StepId) {
        if self.synthetic || self.testing_s3.is_some() {
            return;
        }
        let Some(step) = self.plan.steps.iter().find(|step| step.id == id) else {
            return;
        };
        let Some(checked) = step.checked.as_ref().filter(|checked| checked.ready()) else {
            self.set_error("Complete the destination fields before testing.");
            return;
        };
        match runner::test_s3(
            self.services.clone(),
            checked.clone(),
            self.drafts.snapshot(),
        ) {
            Ok(receive) => {
                self.testing_s3 = Some(PendingS3 {
                    step: id,
                    call: step.call.clone(),
                    receive,
                });
                self.step_outcomes.insert(
                    id,
                    StepStatus {
                        state: StepPhase::Running,
                        message: "Checking the bucket…".into(),
                    },
                );
            }
            Err(error) => self.set_error(error),
        }
    }
    pub(in crate::gui) fn tick(&mut self, now: Instant) {
        if self.synthetic {
            return;
        }
        if let Some(result) = self
            .gathering
            .as_ref()
            .and_then(|receive| match receive.try_recv() {
                Ok(result) => Some(result),
                Err(TryRecvError::Disconnected) => Some(Err(runner::Failure {
                    message: "The session-details worker stopped. Try again.".into(),
                    excerpt: None,
                })),
                Err(TryRecvError::Empty) => None,
            })
        {
            self.gathering = None;
            match result {
                Ok(ctx) => {
                    self.context = Some(ctx);
                    self.recheck();
                }
                Err(error) => {
                    if self.context.is_none() {
                        self.set_error(error.message);
                    }
                }
            }
        }
        loop {
            let next = self.turn.as_ref().map(|turn| turn.receive.try_recv());
            match next {
                Some(Ok(event)) => self.turn_event(event),
                Some(Err(TryRecvError::Disconnected)) => {
                    self.turn.take();
                    self.set_error("The assistant worker stopped. Try the request again.");
                    break;
                }
                _ => break,
            }
        }
        loop {
            let next = self
                .applying
                .as_ref()
                .map(|applying| applying.receive.try_recv());
            match next {
                Some(Ok(event)) => self.apply_event(event),
                Some(Err(TryRecvError::Disconnected)) => {
                    self.applying.take();
                    let completed: Vec<_> = self
                        .step_outcomes
                        .iter()
                        .filter(|(_, outcome)| outcome.state == StepPhase::Done)
                        .map(|(id, outcome)| (*id, outcome.message.clone()))
                        .collect();
                    let before = self.plan.clone();
                    for (id, summary) in completed {
                        let _ = self.plan.remove(id);
                        self.step_outcomes.remove(&id);
                        self.append(TranscriptEntry::Outcome {
                            summary,
                            level: Level::Done,
                        });
                    }
                    self.drafts.reconcile(&before, &self.plan);
                    self.set_error("The plan worker stopped. Check the session before running remaining steps.");
                    self.remaining = true;
                    self.context = None;
                    break;
                }
                _ => break,
            }
        }
        let exported = self
            .exporting
            .as_ref()
            .and_then(|receive| match receive.try_recv() {
                Ok(result) => Some(result),
                Err(TryRecvError::Disconnected) => {
                    Some(Err("The plan export worker stopped.".into()))
                }
                Err(TryRecvError::Empty) => None,
            });
        if let Some(result) = exported {
            self.exporting = None;
            match result {
                Ok(Some(path)) => {
                    self.exported_plan = Some(path);
                    self.notice = Some(("Plan exported.".into(), None, Level::Done));
                }
                Ok(None) => {}
                Err(error) => self.set_error(error),
            }
        }
        if let Some(result) =
            self.testing_s3
                .as_ref()
                .and_then(|pending| match pending.receive.try_recv() {
                    Ok(result) => Some(result),
                    Err(TryRecvError::Disconnected) => {
                        Some(Err("The bucket test worker stopped.".into()))
                    }
                    Err(TryRecvError::Empty) => None,
                })
        {
            let pending = self.testing_s3.take().expect("pending bucket test");
            if self
                .plan
                .steps
                .iter()
                .any(|step| step.id == pending.step && step.call == pending.call)
            {
                self.step_outcomes.insert(
                    pending.step,
                    match result {
                        Ok(message) => StepStatus {
                            state: StepPhase::Done,
                            message,
                        },
                        Err(message) => StepStatus {
                            state: StepPhase::Failed,
                            message,
                        },
                    },
                );
            }
        }
        if self.open
            && self.applying.is_none()
            && self
                .refreshed
                .is_none_or(|at| now.saturating_duration_since(at) >= CONTEXT_INTERVAL)
        {
            self.refresh(now);
        }
    }
    fn turn_event(&mut self, event: runner::TurnEvent) {
        let Some(pending) = &self.turn else {
            return;
        };
        let accept = !pending.cancelled && pending.generation == self.generation;
        if let runner::TurnEvent::Finished(result) = event {
            let pending = self.turn.take().expect("current turn");
            if !accept {
                if let Ok(outcome) = result {
                    let snapshot = self.plan.clone();
                    self.plan = outcome.plan;
                    self.plan.restore_from(&snapshot);
                    self.recheck();
                }
                self.status = "Stopped".into();
                self.notice = Some((
                    "Assistant stopped. The plan was restored.".into(),
                    None,
                    Level::Warning,
                ));
                return;
            }
            match result {
                Ok(outcome) => {
                    if outcome.cancelled {
                        let before = self.plan.clone();
                        self.plan.restore_from(&pending.before);
                        self.drafts.reconcile(&before, &self.plan);
                        self.conversation = None;
                        self.status = "Stopped".into();
                    } else {
                        let before = self.plan.clone();
                        self.plan = outcome.plan;
                        self.drafts.reconcile(&before, &self.plan);
                        self.conversation = Some(outcome.conversation);
                        self.recheck();
                        self.usage = pending.usage_before;
                        self.usage += outcome.usage;
                        self.error = outcome.error;
                        self.status = if self.error.is_some() {
                            "Needs attention"
                        } else {
                            "Ready"
                        }
                        .into();
                        if !outcome.text.is_empty()
                            && !matches!(self.transcript.last(),Some(TranscriptEntry::Assistant(value)) if value==&outcome.text)
                        {
                            self.append(TranscriptEntry::Assistant(bounded(
                                &outcome.text,
                                64 * 1024,
                            )));
                        }
                    }
                    self.notice = Some((
                        if self.error.is_some() {
                            "The assistant needs attention."
                        } else {
                            "The assistant finished its response."
                        }
                        .into(),
                        self.error.clone(),
                        if self.error.is_some() {
                            Level::Warning
                        } else {
                            Level::Done
                        },
                    ));
                }
                Err(error) => {
                    self.error = Some(error.message.clone());
                    self.provider_excerpt = error.excerpt;
                    self.status = "Failed".into();
                    self.notice = Some((
                        "The assistant request failed.".into(),
                        Some(error.message),
                        Level::Error,
                    ));
                }
            }
            return;
        }
        if !accept {
            if let runner::TurnEvent::Plan(plan) = event {
                let snapshot = self.plan.clone();
                self.plan = plan;
                self.plan.restore_from(&snapshot);
                self.recheck();
            }
            return;
        }
        match event {
            runner::TurnEvent::Status(value) => {
                self.status = tools::redact::for_output_text(&value)
            }
            runner::TurnEvent::Text(value) => self.append(TranscriptEntry::Assistant(bounded(
                &tools::redact::for_output_text(&value),
                64 * 1024,
            ))),
            runner::TurnEvent::Tool {
                call,
                output,
                images,
            } => {
                let title = tools::spec(&call.tool)
                    .map_or_else(|| call.tool.clone(), |spec| spec.title.into());
                let summary = bounded(&tools::redact::for_output(output).to_string(), 2048);
                self.append(TranscriptEntry::Tool {
                    title,
                    summary,
                    images,
                });
            }
            runner::TurnEvent::Plan(plan) => {
                let before = self.plan.clone();
                self.plan = plan;
                self.drafts.reconcile(&before, &self.plan);
                self.recheck();
            }
            runner::TurnEvent::Usage(usage) => {
                self.usage = self.turn.as_ref().expect("current turn").usage_before;
                self.usage += usage;
            }
            runner::TurnEvent::Notice(message) => self.append(TranscriptEntry::Notice(bounded(
                &tools::redact::for_output_text(&message),
                4096,
            ))),
            runner::TurnEvent::Image(image) => self.append(TranscriptEntry::Tool {
                title: "Inspected camera image".into(),
                summary: image.part.label.clone(),
                images: vec![image],
            }),
            runner::TurnEvent::Finished(_) => {}
        }
    }
    fn apply_event(&mut self, event: runner::ApplyEvent) {
        match event {
            runner::ApplyEvent::Progress(progress) => {
                let Some(applying) = &self.applying else {
                    return;
                };
                let Some((id, _)) = progress
                    .step
                    .checked_sub(1)
                    .and_then(|index| applying.calls.get(index))
                else {
                    return;
                };
                let phase = match progress.state {
                    ProgressState::Completed => StepPhase::Done,
                    _ => StepPhase::Running,
                };
                self.step_outcomes.insert(
                    *id,
                    StepStatus {
                        state: phase,
                        message: progress.summary.clone(),
                    },
                );
                self.status = progress.summary;
            }
            runner::ApplyEvent::Finished { result, context } => {
                let Some(applying) = self.applying.take() else {
                    return;
                };
                if let Some(ctx) = context {
                    self.context = Some(*ctx);
                } else {
                    self.context = None;
                }
                let (completed, error, failed) = match result {
                    Ok(value) => (
                        value["completed"].as_array().cloned().unwrap_or_default(),
                        None,
                        None,
                    ),
                    Err(tools::ApplyError::Check(_)) => (
                        vec![],
                        Some(
                            "The plan changed since you reviewed it. Check it and run again."
                                .to_owned(),
                        ),
                        None,
                    ),
                    Err(tools::ApplyError::Run(failure)) => (
                        failure.completed,
                        Some(tools::redact::for_output_text(&format!(
                            "{:#}",
                            failure.source
                        ))),
                        failure.failed_step.checked_sub(1),
                    ),
                };
                let before = self.plan.clone();
                for (index, value) in completed.iter().enumerate() {
                    if let Some((id, _)) = applying.calls.get(index) {
                        let summary = value["summary"].as_str().unwrap_or("Step completed.");
                        self.append(TranscriptEntry::Outcome {
                            summary: tools::redact::for_output_text(summary),
                            level: Level::Done,
                        });
                        let _ = self.plan.remove(*id);
                        self.step_outcomes.remove(id);
                    }
                }
                self.drafts.reconcile(&before, &self.plan);
                if let (Some(index), Some(error)) = (failed, &error)
                    && let Some((id, _)) = applying.calls.get(index)
                {
                    self.step_outcomes.insert(
                        *id,
                        StepStatus {
                            state: StepPhase::Failed,
                            message: error.clone(),
                        },
                    );
                }
                self.remaining = error.is_some() && !self.plan.steps.is_empty();
                self.error = error;
                self.recheck();
                let stopped = applying.cancel.load(Ordering::SeqCst);
                self.status = if stopped {
                    "Stopped"
                } else if self.error.is_some() {
                    "Needs attention"
                } else {
                    "Done"
                }
                .into();
                let (message, level) = if stopped {
                    (
                        "The plan stopped. Completed steps were kept.",
                        Level::Warning,
                    )
                } else if self.error.is_some() {
                    (
                        "The plan needs attention. Completed steps were kept.",
                        Level::Warning,
                    )
                } else {
                    ("The assistant plan finished.", Level::Done)
                };
                self.notice = Some((message.into(), self.error.clone(), level));
            }
        }
    }
}

fn approval_view(plan: &Plan) -> Value {
    json!(plan.steps.iter().map(|step|json!({"id":step.id,"call":step.call,"checked":step.checked.as_ref().map(|checked|{
        let mut value=checked.value();value["facts"]=json!(tools::exec::approval_facts(checked));value
    })})).collect::<Vec<_>>())
}
fn bounded(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.into();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}
fn transcript_bytes(entries: &[TranscriptEntry]) -> usize {
    entries
        .iter()
        .map(|entry| match entry {
            TranscriptEntry::User(value)
            | TranscriptEntry::Assistant(value)
            | TranscriptEntry::Notice(value) => value.len(),
            TranscriptEntry::Tool { title, summary, .. } => title.len() + summary.len(),
            TranscriptEntry::Outcome { summary, .. } => summary.len(),
        })
        .sum()
}
fn image_bytes(entries: &[TranscriptEntry]) -> usize {
    entries
        .iter()
        .map(|entry| match entry {
            TranscriptEntry::Tool { images, .. } => images.iter().map(image_footprint).sum(),
            _ => 0,
        })
        .sum()
}
fn image_footprint(image: &SentImage) -> usize {
    image
        .part
        .data
        .len()
        .saturating_add(if image.handle.is_some() {
            (image.part.width as usize)
                .saturating_mul(image.part.height as usize)
                .saturating_mul(4)
        } else {
            0
        })
}

#[cfg(test)]
#[path = "quick_tests.rs"]
mod tests;
