//! Bounded camera tasks. The model proposes calls; Rust owns scope, budgets,
//! approvals, attribution and every decision about whether a call may execute.
use super::{
    agent::{Deps, Event, ModelSession},
    approval::{ApprovalDecision, ApprovalPolicy, ApprovalRequest, Approver},
    conversation::{CallRecord, Conversation, ImagePart, ResultRecord, Stop},
    limits::{Limiter, RequestProjection, ToolImpact, Totals},
    log::ActionLog,
    prompt,
    protocol::{self, Mode},
    provider::{Protocol, ProviderId},
    segment::{self, Checkpoint, Recovery, RecoveryTracker, SegmentStats},
};
pub use super::{approval::RunMode, limits::Limits};
use crate::{
    session::{ActivityState, ActorKind, SessionActor, SessionCommand, SessionSnapshot},
    tools::{
        self, Checked, Executor, Origin, PlanState, Risk, RunFacts, StepId, ToolCall, ToolKind,
        context::{Context, ToolHost},
        read::{ReadOptions, TurnCache},
        redact,
    },
};
use anyhow::{Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Digest as _;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct RunScope {
    pub cameras: Vec<String>,
    pub destinations: Vec<String>,
    pub allow_images: bool,
    pub allow_recordings: bool,
    pub allow_schedules: bool,
    pub effects_may_outlive_run: bool,
}

impl RunScope {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=8).contains(&self.cameras.len()),
            "Choose from one to eight cameras for this task."
        );
        let unique: BTreeSet<_> = self.cameras.iter().collect();
        ensure!(
            unique.len() == self.cameras.len()
                && self.cameras.iter().all(|camera| !camera.trim().is_empty()),
            "Choose each camera only once."
        );
        ensure!(
            self.destinations.iter().all(|name| !name.trim().is_empty()),
            "Choose saved destinations by name."
        );
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OnHumanChange {
    #[default]
    Pause,
    Inform,
    Stop,
}

#[derive(Clone, Debug)]
pub struct RunRequest {
    pub goal: String,
    pub mode: RunMode,
    pub scope: RunScope,
    pub limits: Limits,
    pub on_human_change: OnHumanChange,
    pub provider: Option<ProviderId>,
    pub model: Option<String>,
    /// Desktop tasks can wait for a follow-up; a noninteractive CLI returns
    /// a needs_user outcome instead of waiting without an input channel.
    pub wait_for_answer: bool,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    #[default]
    Starting,
    Running,
    Waiting,
    WaitingApproval,
    Paused,
    Stopping,
    Finished,
    NeedsUser,
    Stopped,
    LimitReached,
    Failed,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct RunSnapshot {
    pub run_id: String,
    pub actor_id: String,
    pub state: RunState,
    pub round: u32,
    pub elapsed_ms: u64,
    pub summary: String,
    pub totals: Totals,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Finished,
    NeedsUser,
    Stopped,
    Limit,
    Failed,
    AppQuit,
}

#[derive(Clone, Debug, Serialize)]
pub struct SettingChange {
    pub camera: String,
    pub feature: String,
    pub before: String,
    pub last: String,
    pub needs_review: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct RunRecord {
    pub run_id: String,
    pub actor_id: String,
    pub settings: Vec<SettingChange>,
    pub worker_pids: BTreeMap<String, u32>,
    /// Opaque worker identity for recordings this task actually started.
    pub forwarders: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Leftover {
    pub kind: String,
    pub camera: String,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub until_ms: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct RunOutcome {
    pub run_id: String,
    pub outcome: Outcome,
    pub summary: String,
    pub totals: Totals,
    pub leftovers: Vec<Leftover>,
    pub log: PathBuf,
    pub restore: RunRecord,
}

#[derive(Clone, Debug)]
pub enum RunEvent {
    Agent(Event),
    State(RunSnapshot),
    Approval(ApprovalRequest),
    Changes(Vec<SettingChange>),
    Finished(RunOutcome),
}

struct Shared {
    cancel: AtomicBool,
    pause: AtomicBool,
    generation: AtomicU64,
    messages: Mutex<VecDeque<String>>,
    status: Mutex<RunSnapshot>,
    completion: Mutex<Option<std::result::Result<RunOutcome, String>>>,
    log: Mutex<Option<Arc<Mutex<ActionLog>>>>,
    end_fields: Mutex<Value>,
    ended: AtomicBool,
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            cancel: AtomicBool::new(false),
            pause: AtomicBool::new(false),
            generation: AtomicU64::new(1),
            messages: Mutex::new(VecDeque::new()),
            status: Mutex::new(RunSnapshot::default()),
            completion: Mutex::new(None),
            log: Mutex::new(None),
            end_fields: Mutex::new(json!({})),
            ended: AtomicBool::new(false),
        }
    }
}

#[derive(Clone, Default)]
pub struct RunControl {
    shared: Arc<Shared>,
}

impl RunControl {
    pub fn stop(&self) {
        if self.shared.ended.load(Ordering::SeqCst) {
            return;
        }
        let already = self.shared.cancel.swap(true, Ordering::SeqCst);
        self.shared.generation.fetch_add(1, Ordering::SeqCst);
        if !already
            && let Some(log) = self
                .shared
                .log
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .as_ref()
        {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX);
            let _ = log
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .record("stop", json!({"by":"caller","reason":"requested"}), now);
        }
        if let Ok(mut status) = self.shared.status.lock()
            && matches!(
                status.state,
                RunState::Starting
                    | RunState::Running
                    | RunState::Waiting
                    | RunState::WaitingApproval
                    | RunState::Paused
            )
        {
            status.state = RunState::Stopping;
            status.summary = "Finishing the current camera command…".into();
        }
    }

    pub fn pause(&self) {
        self.shared.pause.store(true, Ordering::SeqCst);
    }
    pub fn resume(&self) {
        self.shared.pause.store(false, Ordering::SeqCst);
    }
    pub fn message(&self, text: impl Into<String>) -> Result<()> {
        let text = text.into();
        prompt::validate_prompt(&text)?;
        ensure!(
            !self.shared.cancel.load(Ordering::SeqCst) && !self.shared.ended.load(Ordering::SeqCst),
            "This assistant task has ended."
        );
        let mut messages = self
            .shared
            .messages
            .lock()
            .map_err(|_| anyhow!("The task message queue is unavailable."))?;
        ensure!(
            messages.len() < 16,
            "Wait for the assistant to read your earlier messages."
        );
        messages.push_back(text);
        Ok(())
    }

    pub fn status(&self) -> RunSnapshot {
        self.shared
            .status
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
    pub fn result(&self) -> Option<std::result::Result<RunOutcome, String>> {
        self.shared
            .completion
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
    pub fn is_finished(&self) -> bool {
        self.shared.ended.load(Ordering::SeqCst)
    }
    pub fn cancel(&self) -> &AtomicBool {
        &self.shared.cancel
    }
    /// Called synchronously before the desktop starts dropping worker
    /// messages during shutdown. It never waits for a provider thread.
    pub fn finish_app_quit(&self) {
        // Claim the terminal outcome before publishing cancellation: the
        // worker may finish immediately after it observes that flag.
        if self.shared.ended.swap(true, Ordering::SeqCst) {
            return;
        }
        let already = self.shared.cancel.swap(true, Ordering::SeqCst);
        self.shared.generation.fetch_add(1, Ordering::SeqCst);
        if let Some(log) = self
            .shared
            .log
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
        {
            let mut fields = self
                .shared
                .end_fields
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone();
            fields["outcome"] = json!("app_quit");
            fields["summary"] = json!("The assistant stopped because Capturefab quit.");
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX);
            let mut log = log.lock().unwrap_or_else(|error| error.into_inner());
            if !already {
                let _ = log.record("stop", json!({"by":"caller","reason":"requested"}), now);
            }
            let _ = log.finish(fields, now);
        }
    }
}

#[derive(Clone, Default)]
pub struct RunHandle {
    pub control: RunControl,
}

impl RunHandle {
    pub fn stop(&self) {
        self.control.stop();
    }
    pub fn pause(&self) {
        self.control.pause();
    }
    pub fn resume(&self) {
        self.control.resume();
    }
    pub fn message(&self, text: impl Into<String>) -> Result<()> {
        self.control.message(text)
    }
    pub fn status(&self) -> RunSnapshot {
        self.control.status()
    }
    pub fn result(&self) -> Option<std::result::Result<RunOutcome, String>> {
        self.control.result()
    }
    pub fn is_finished(&self) -> bool {
        self.control.is_finished()
    }
    pub fn finish_app_quit(&self) {
        self.control.finish_app_quit();
    }
}

pub struct Run;

impl Run {
    pub fn start(
        request: RunRequest,
        deps: Arc<Deps>,
        approver: Arc<dyn Approver>,
        mut events: impl FnMut(RunEvent) + Send + 'static,
    ) -> Result<RunHandle> {
        prompt::validate_prompt(&request.goal)?;
        request.scope.validate()?;
        let control = RunControl::default();
        let worker = control.clone();
        thread::Builder::new()
            .name("capturefab-assistant".into())
            .spawn(move || {
                let result = run_blocking(request, &deps, approver, &worker, &mut events);
                if let Err(error) = &result {
                    let mut status = worker
                        .shared
                        .status
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    status.state = RunState::Failed;
                    status.summary = redact::for_output_text(&error.to_string());
                    events(RunEvent::State(status.clone()));
                }
                *worker
                    .shared
                    .completion
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) =
                    Some(result.map_err(|error| redact::for_output_text(&error.to_string())));
                worker.shared.ended.store(true, Ordering::SeqCst);
            })?;
        Ok(RunHandle { control })
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct RestorePlan {
    pub calls: Vec<ToolCall>,
    pub skipped: Vec<RestoreSkipped>,
}

#[derive(Clone, Debug, Serialize)]
pub struct RestoreSkipped {
    pub camera: String,
    pub feature: String,
    pub reason: String,
}

/// Restore is a proposal for a human action. The caller reviews it and sends
/// it with the human actor; it never replays somebody else's undo entry.
pub fn restore(record: &RunRecord, ctx: &Context) -> RestorePlan {
    let mut plan = RestorePlan::default();
    let mut cameras: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for setting in &record.settings {
        let camera = ctx
            .snapshot
            .cameras
            .iter()
            .find(|camera| camera.info.id == setting.camera);
        let current = camera
            .and_then(|camera| {
                camera
                    .features
                    .iter()
                    .find(|feature| feature.name == setting.feature)
            })
            .and_then(|feature| feature.value.as_ref())
            .map(value_text);
        let same_worker = camera.is_some_and(|camera| {
            record.worker_pids.get(&setting.camera) == Some(&camera.worker_pid)
        });
        let reason = if setting.needs_review {
            Some("Selector-dependent settings need review in the camera inspector.")
        } else if !same_worker {
            Some("The camera was reconnected after this task.")
        } else if current.as_deref() != Some(setting.last.as_str()) {
            Some("Changed after the assistant; not restored.")
        } else {
            None
        };
        if let Some(reason) = reason {
            plan.skipped.push(RestoreSkipped {
                camera: setting.camera.clone(),
                feature: setting.feature.clone(),
                reason: reason.into(),
            });
        } else if setting.before != setting.last {
            cameras.entry(setting.camera.clone()).or_default().push(
                json!({"feature":setting.feature,"value":setting.before,"expect":setting.last}),
            );
        }
    }
    for (camera, values) in cameras {
        for chunk in values.chunks(16) {
            plan.calls.push(ToolCall {
                tool: "set_camera_features".into(),
                args: json!({"camera":camera,"values":chunk,"restart_stream":false}),
            });
        }
    }
    plan
}

#[derive(Default)]
struct HumanTracker {
    activities: BTreeMap<u64, String>,
    values: BTreeMap<(String, String), String>,
    workers: BTreeMap<String, u32>,
}

fn value_text(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string())
}

fn values(ctx: &Context, scope: &BTreeSet<String>) -> BTreeMap<(String, String), String> {
    ctx.snapshot
        .cameras
        .iter()
        .filter(|camera| scope.contains(&camera.info.id))
        .flat_map(|camera| {
            camera
                .features
                .iter()
                .filter(|feature| !tools::risk::managed(&feature.name, None, Some(&feature.kind)))
                .filter_map(|feature| {
                    feature.value.as_ref().map(|value| {
                        (
                            (camera.info.id.clone(), feature.name.clone()),
                            value_text(value),
                        )
                    })
                })
        })
        .collect()
}

fn auto_managed(snapshot: &SessionSnapshot, camera: &str, feature: &str) -> bool {
    let Some(camera) = snapshot
        .cameras
        .iter()
        .find(|state| state.info.id == camera)
    else {
        return false;
    };
    if camera
        .auto
        .as_ref()
        .is_some_and(|auto| auto.state != "off" && auto.managed.iter().any(|name| name == feature))
    {
        return true;
    }
    let native = |name: &str| {
        camera
            .features
            .iter()
            .find(|info| info.name == name)
            .and_then(|info| info.value.as_ref())
            .is_some_and(|value| {
                !matches!(
                    value_text(value).to_ascii_lowercase().as_str(),
                    "off" | "false" | "0"
                )
            })
    };
    match feature {
        "ExposureTime" | "ExposureTimeAbs" => native("ExposureAuto"),
        "Gain" | "GainAbs" => native("GainAuto"),
        "BalanceRatio" => native("BalanceWhiteAuto"),
        _ => false,
    }
}

fn activity_signature(activity: &crate::session::SessionActivity) -> String {
    format!(
        "{:?}:{:?}:{:?}",
        activity.state, activity.started_at_ms, activity.finished_at_ms
    )
}

fn human_operation(operation: &str) -> bool {
    matches!(
        operation,
        "set"
            | "set_many"
            | "execute"
            | "auto"
            | "manual"
            | "start"
            | "stop"
            | "forward"
            | "stop_forward"
            | "cancel_job"
            | "disconnect"
            | "replay_camera_change"
            | "undo"
            | "redo"
            | "connect"
            | "select"
    )
}

impl HumanTracker {
    fn new(ctx: &Context, scope: &BTreeSet<String>) -> Self {
        Self {
            activities: ctx
                .snapshot
                .activity
                .iter()
                .map(|activity| (activity.id, activity_signature(activity)))
                .collect(),
            values: values(ctx, scope),
            workers: ctx
                .snapshot
                .cameras
                .iter()
                .filter(|camera| scope.contains(&camera.info.id))
                .map(|camera| (camera.info.id.clone(), camera.worker_pid))
                .collect(),
        }
    }

    fn changes(
        &self,
        ctx: &Context,
        scope: &BTreeSet<String>,
        actor: &str,
    ) -> Vec<(String, Option<String>, String)> {
        let mut changes = Vec::new();
        for camera in scope {
            let worker = ctx
                .snapshot
                .cameras
                .iter()
                .find(|state| state.info.id == *camera)
                .map(|state| state.worker_pid);
            if self.workers.get(camera).copied() != worker {
                changes.push((
                    camera.clone(),
                    None,
                    format!(
                        "{} was connected, disconnected or reconnected.",
                        redact::for_model_text(camera)
                    ),
                ));
            }
        }
        for activity in &ctx.snapshot.activity {
            if activity.state == ActivityState::Failed
                || self.activities.get(&activity.id) == Some(&activity_signature(activity))
                || !human_operation(&activity.operation)
                || activity.actor.as_ref().is_some_and(|by| by.id == actor)
            {
                continue;
            }
            if let Some(camera) = activity
                .camera
                .as_ref()
                .filter(|camera| scope.contains(*camera))
            {
                changes.push((
                    camera.clone(),
                    activity.feature.clone(),
                    redact::for_model_text(&activity.label),
                ));
            }
        }
        for ((camera, feature), current) in values(ctx, scope) {
            if self
                .values
                .get(&(camera.clone(), feature.clone()))
                .is_some_and(|previous| previous != &current)
                && !auto_managed(&ctx.snapshot, &camera, &feature)
                && !changes
                    .iter()
                    .any(|(id, name, _)| id == &camera && name.as_deref() == Some(&feature))
            {
                changes.push((
                    camera.clone(),
                    Some(feature.clone()),
                    format!(
                        "{feature} changed on {} to {}.",
                        redact::for_model_text(&camera),
                        redact::for_model_text(&current)
                    ),
                ));
            }
        }
        changes
    }

    fn accept(&mut self, ctx: &Context, scope: &BTreeSet<String>) {
        self.activities = ctx
            .snapshot
            .activity
            .iter()
            .map(|activity| (activity.id, activity_signature(activity)))
            .collect();
        self.values = values(ctx, scope);
        self.workers = ctx
            .snapshot
            .cameras
            .iter()
            .filter(|camera| scope.contains(&camera.info.id))
            .map(|camera| (camera.info.id.clone(), camera.worker_pid))
            .collect();
    }
}

struct Runtime<'a> {
    request: RunRequest,
    deps: &'a Deps,
    host: Arc<dyn ToolHost>,
    control: &'a RunControl,
    events: &'a mut dyn FnMut(RunEvent),
    model: ModelSession,
    conversation: Conversation,
    limiter: Limiter,
    log: Arc<Mutex<ActionLog>>,
    policy: ApprovalPolicy,
    facts: RunFacts,
    record: RunRecord,
    context: Context,
    humans: HumanTracker,
    owned_forwarders: BTreeMap<String, String>,
    uncertain_forwarders: BTreeSet<String>,
    owned_auto: BTreeSet<String>,
    notes: Vec<String>,
    experiments: Vec<String>,
    decisions: Vec<String>,
    human_notes: Vec<String>,
    pending_human_notes: Vec<String>,
    last_text: String,
    next_step: u64,
    next_approval: u64,
    segment: u32,
    segment_rounds: u32,
    recovery: RecoveryTracker,
    turn: TurnCache,
    start_ms: u64,
    checkpoint_images: Vec<ImagePart>,
    last_request_ms: Option<u64>,
}

fn scope_ids(
    scope: &RunScope,
    aliases: &std::collections::HashMap<String, String>,
) -> BTreeSet<String> {
    scope
        .cameras
        .iter()
        .flat_map(|camera| std::iter::once(camera.clone()).chain(aliases.get(camera).cloned()))
        .collect()
}

/// Camera IDs and schedule IDs may be reused by a replacement worker. No
/// permission or restore baseline from the old lifetime transfers to it.
fn forget_worker(
    camera: &str,
    facts: &mut RunFacts,
    record: &mut RunRecord,
    tokens: &mut Vec<(String, String)>,
    policy: &mut ApprovalPolicy,
    forwarders: &mut BTreeMap<String, String>,
    auto: &mut BTreeSet<String>,
) {
    facts.jobs.retain(|(id, _)| id != camera);
    facts.tokens.retain(|(id, _)| id != camera);
    tokens.retain(|(id, _)| id != camera);
    facts.forwarders.remove(camera);
    facts.worker_pids.remove(camera);
    record.worker_pids.remove(camera);
    record.forwarders.remove(camera);
    forwarders.remove(camera);
    auto.remove(camera);
    policy.invalidate_camera(camera);
    for setting in record
        .settings
        .iter_mut()
        .filter(|setting| setting.camera == camera)
    {
        setting.needs_review = true;
    }
}

fn pin_scope(scope: &RunScope, ctx: &Context) -> Result<RunScope> {
    let mut pinned = scope.clone();
    pinned.cameras = scope
        .cameras
        .iter()
        .map(|camera| {
            tools::resolve_camera(ctx, "camera", camera, true)
                .map(|camera| camera.selector().to_owned())
                .map_err(|issue| anyhow!(issue.message))
        })
        .collect::<Result<_>>()?;
    let mut names = Vec::new();
    for name in &scope.destinations {
        let destination = ctx
            .destinations
            .iter()
            .find(|destination| destination.name.eq_ignore_ascii_case(name))
            .ok_or_else(|| {
                anyhow!(
                    "{} is not an available destination.",
                    redact::for_output_text(name)
                )
            })?;
        names.push(destination.name.clone());
    }
    pinned.destinations = names;
    pinned.validate()?;
    Ok(pinned)
}

const MAX_INITIALIZATIONS: usize = 16;
static INITIALIZATIONS: AtomicUsize = AtomicUsize::new(0);
struct InitializationSlot;
impl InitializationSlot {
    fn acquire() -> Result<Self> {
        loop {
            let count = INITIALIZATIONS.load(Ordering::SeqCst);
            ensure!(
                count < MAX_INITIALIZATIONS,
                "Earlier credential lookups are still finishing. Wait before starting another task."
            );
            if INITIALIZATIONS
                .compare_exchange(count, count + 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                break;
            }
        }
        Ok(Self)
    }
}
impl Drop for InitializationSlot {
    fn drop(&mut self) {
        INITIALIZATIONS.fetch_sub(1, Ordering::SeqCst);
    }
}

/// OS credential calls can finish late. The parent waits only within the run
/// budget; cancellation stops later auth/probe stages and no camera commands
/// are available to this helper. A process-wide cap bounds uncancellable OS
/// calls without retaining their credentials in the UI.
fn initialize_model(
    deps: &Deps,
    provider: ProviderId,
    settings: super::provider::ProviderSettings,
    allow_images: bool,
    deadline_ms: u64,
    control: &RunControl,
) -> Result<Option<ModelSession>> {
    credential_work(deps, deadline_ms, control, move |deps, worker| {
        ModelSession::resolve_until(
            deps,
            provider,
            settings,
            Mode::Run,
            allow_images,
            deadline_ms,
            worker.cancel(),
        )
    })
}

fn credential_work<T: Send + 'static>(
    deps: &Deps,
    deadline_ms: u64,
    control: &RunControl,
    resolve: impl FnOnce(&Deps, &RunControl) -> Result<T> + Send + 'static,
) -> Result<Option<T>> {
    let slot = InitializationSlot::acquire()?;
    let deps = deps.clone();
    let clock = deps.clock.clone();
    let worker = control.clone();
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("capturefab-assistant-auth".into())
        .spawn(move || {
            let _slot = slot;
            let result = resolve(&deps, &worker);
            let _ = sender.send(result);
        })?;
    loop {
        if control.cancel().load(Ordering::SeqCst) || clock.now_ms() >= deadline_ms {
            control.shared.cancel.store(true, Ordering::SeqCst);
            return Ok(None);
        }
        let remaining = deadline_ms.saturating_sub(clock.now_ms());
        match receiver.recv_timeout(Duration::from_millis(remaining.min(250))) {
            Ok(result) => return result.map(Some),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                bail!("The assistant credential worker disconnected.")
            }
        }
    }
}

#[derive(Clone, Copy)]
struct InitializationLog<'a> {
    run_id: &'a str,
    actor: &'a SessionActor,
    log: &'a Arc<Mutex<ActionLog>>,
    deps: &'a Deps,
    control: &'a RunControl,
    start_ms: u64,
}
impl InitializationLog<'_> {
    fn finish(
        self,
        outcome: Outcome,
        summary: String,
        events: &mut dyn FnMut(RunEvent),
    ) -> Result<RunOutcome> {
        let Self {
            run_id,
            actor,
            log,
            deps,
            control,
            start_ms,
        } = self;
        let outcome = if control.shared.ended.load(Ordering::SeqCst) {
            Outcome::AppQuit
        } else {
            outcome
        };
        let summary = if outcome == Outcome::AppQuit {
            "The assistant stopped because Capturefab quit.".into()
        } else {
            summary
        };
        let report = RunOutcome {
            run_id: run_id.into(),
            outcome,
            summary: summary.clone(),
            totals: Totals::default(),
            leftovers: Vec::new(),
            log: log
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .path()
                .to_owned(),
            restore: RunRecord {
                run_id: run_id.into(),
                actor_id: actor.id.clone(),
                ..RunRecord::default()
            },
        };
        if !control.shared.ended.swap(true, Ordering::SeqCst) {
            log.lock().map_err(|_|anyhow!("The assistant action log is unavailable."))?.finish(json!({"outcome":outcome,"summary":summary,"totals":report.totals,"leftovers":report.leftovers}),deps.clock.now_ms())?;
        }
        let snapshot = RunSnapshot {
            run_id: run_id.into(),
            actor_id: actor.id.clone(),
            state: match outcome {
                Outcome::Stopped | Outcome::AppQuit => RunState::Stopped,
                Outcome::Limit => RunState::LimitReached,
                _ => RunState::Failed,
            },
            elapsed_ms: deps.clock.now_ms().saturating_sub(start_ms),
            summary,
            ..RunSnapshot::default()
        };
        *control
            .shared
            .status
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = snapshot.clone();
        events(RunEvent::State(snapshot));
        events(RunEvent::Finished(report.clone()));
        Ok(report)
    }
}

pub fn run_blocking(
    mut request: RunRequest,
    deps: &Deps,
    approver: Arc<dyn Approver>,
    control: &RunControl,
    events: &mut dyn FnMut(RunEvent),
) -> Result<RunOutcome> {
    let start_ms = deps.clock.now_ms();
    ensure!(
        !control.cancel().load(Ordering::SeqCst),
        "The assistant task was stopped."
    );
    prompt::validate_prompt(&request.goal)?;
    request.scope.validate()?;
    let initial = deps.gather(deps.host.as_ref())?;
    ensure!(
        initial.session,
        "An assistant task needs a running session."
    );
    request.scope = pin_scope(&request.scope, &initial)?;
    let provider = request
        .provider
        .or(deps.settings.provider)
        .ok_or_else(|| anyhow!("Choose an AI provider in Settings first."))?;
    let mut settings = deps.settings.provider_settings(provider);
    if let Some(model) = &request.model {
        settings.model = Some(model.clone());
    }
    let prices = deps
        .settings
        .prices
        .get(&format!("{}/{}", provider.id(), settings.model(provider)))
        .copied();
    let ceiling = initial
        .storage
        .free_budget()
        .min(initial.storage.free_disk.unwrap_or(u64::MAX));
    let limiter = Limiter::new(request.limits.clone(), start_ms, ceiling, prices)?;
    limiter.check_wall(deps.clock.now_ms())?;
    // Quit takes this same lock before writing its final record. Either no
    // journal starts after cancellation, or quit sees the attached journal and
    // finishes it synchronously before returning to the desktop close filter.
    let (log, run_id, actor) = {
        let mut slot = control
            .shared
            .log
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        ensure!(
            !control.cancel().load(Ordering::SeqCst)
                && !control.shared.ended.load(Ordering::SeqCst),
            "The assistant task was stopped."
        );
        let mut logger = ActionLog::create(&deps.log_directory, start_ms)?;
        let run_id = logger.id().to_owned();
        let run8 = format!("{:x}", sha2::Sha256::digest(run_id.as_bytes()))[..8].to_owned();
        let actor = SessionActor::new(
            format!("assistant:{}:{run8}", std::process::id()),
            "Assistant",
            ActorKind::Agent,
        )?;
        logger.record("start",json!({"goal":request.goal,"mode":request.mode,"scope":request.scope,"limits":request.limits,"provider":provider,"model":settings.model(provider),"effort":settings.effort,"app_version":env!("CARGO_PKG_VERSION"),"actor":actor}),start_ms)?;
        let log = Arc::new(Mutex::new(logger));
        *slot = Some(log.clone());
        *control
            .shared
            .status
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = RunSnapshot {
            run_id: run_id.clone(),
            actor_id: actor.id.clone(),
            state: RunState::Starting,
            summary: "Connecting to the assistant provider…".into(),
            ..RunSnapshot::default()
        };
        (log, run_id, actor)
    };
    // Every failure after the start record must close the log, including
    // failures gathering the attributed host's first snapshot.
    struct EndGuard<'a> {
        log: Arc<Mutex<ActionLog>>,
        deps: &'a Deps,
    }
    impl Drop for EndGuard<'_> {
        fn drop(&mut self) {
            if let Ok(mut log) = self.log.lock() {
                let _ = log.finish(json!({"outcome":"failed","summary":"The assistant task ended before initialization completed."}), self.deps.clock.now_ms());
            }
        }
    }
    let _end_guard = EndGuard {
        log: log.clone(),
        deps,
    };
    let initialization = InitializationLog {
        run_id: &run_id,
        actor: &actor,
        log: &log,
        deps,
        control,
        start_ms,
    };
    if control.cancel().load(Ordering::SeqCst) {
        return initialization.finish(
            Outcome::Stopped,
            "The assistant task was stopped.".into(),
            events,
        );
    }
    events(RunEvent::State(control.status()));
    let initialized = initialize_model(
        deps,
        provider,
        settings,
        request.scope.allow_images,
        limiter.deadline_ms(),
        control,
    );
    let terminal = if control.shared.ended.load(Ordering::SeqCst) {
        Some((
            Outcome::AppQuit,
            "The assistant stopped because Capturefab quit.".into(),
        ))
    } else if deps.clock.now_ms() >= limiter.deadline_ms() {
        control.shared.cancel.store(true, Ordering::SeqCst);
        Some((
            Outcome::Limit,
            "The assistant task reached its time limit.".into(),
        ))
    } else if control.cancel().load(Ordering::SeqCst) {
        Some((Outcome::Stopped, "The assistant task was stopped.".into()))
    } else {
        None
    };
    if let Some((outcome, summary)) = terminal {
        return initialization.finish(outcome, summary, events);
    }
    let model = match initialized {
        Ok(Some(model)) => model,
        Ok(None) => {
            return initialization.finish(
                Outcome::Stopped,
                "The assistant task was stopped.".into(),
                events,
            );
        }
        Err(error) => {
            initialization.finish(
                Outcome::Failed,
                redact::for_output_text(&error.to_string()),
                events,
            )?;
            return Err(error);
        }
    };
    let host = match deps.host.with_actor(actor.clone()) {
        Ok(host) => host,
        Err(error) => {
            let _ = log
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .finish(
                    json!({"outcome":"failed","summary":error.to_string()}),
                    deps.clock.now_ms(),
                );
            return Err(error);
        }
    };
    let ctx = deps.gather(host.as_ref())?;
    let facts = RunFacts {
        actor_id: actor.id.clone(),
        scope: request.scope.cameras.iter().cloned().collect(),
        worker_pids: ctx
            .snapshot
            .cameras
            .iter()
            .filter(|camera| request.scope.cameras.contains(&camera.info.id))
            .map(|camera| (camera.info.id.clone(), camera.worker_pid))
            .collect(),
        ..RunFacts::default()
    };
    let record = RunRecord {
        run_id: run_id.clone(),
        actor_id: actor.id.clone(),
        worker_pids: facts.worker_pids.clone(),
        ..RunRecord::default()
    };
    let mut conversation = Conversation::new(
        provider,
        &model.model,
        prompt::system(Mode::Run),
        protocol::assistant_tools(Mode::Run),
    );
    let goal = format!(
        "{}\nTask scope and limits (data): {}",
        request.goal,
        redact::for_model(json!({"scope":request.scope,"limits":request.limits}))
    );
    conversation.append_user(&prompt::user_prefix(&goal, &ctx), model.wire.as_ref())?;
    let humans = HumanTracker::new(&ctx, &facts.scope);
    let policy = ApprovalPolicy::new(request.mode);
    let mut runtime = Runtime {
        request,
        deps,
        host: host.clone(),
        control,
        events,
        model,
        conversation,
        limiter,
        log,
        policy,
        facts,
        record,
        context: ctx,
        humans,
        owned_forwarders: BTreeMap::new(),
        uncertain_forwarders: BTreeSet::new(),
        owned_auto: BTreeSet::new(),
        notes: Vec::new(),
        experiments: Vec::new(),
        decisions: Vec::new(),
        human_notes: Vec::new(),
        pending_human_notes: Vec::new(),
        last_text: String::new(),
        next_step: 1,
        next_approval: 1,
        segment: 1,
        segment_rounds: 0,
        recovery: RecoveryTracker::default(),
        turn: TurnCache::new(),
        start_ms,
        checkpoint_images: Vec::new(),
        last_request_ms: None,
    };
    let mut executor = Executor::new(host.as_ref(), deps.secrets.as_ref());
    let result = runtime.run(&mut executor, approver);
    let report = match &result {
        Ok((outcome, summary)) => runtime.finish(*outcome, summary.clone()),
        Err(error) => runtime.finish(Outcome::Failed, redact::for_output_text(&error.to_string())),
    }?;
    match result {
        Ok(_) => Ok(report),
        Err(error) => Err(error),
    }
}

impl Runtime<'_> {
    fn id(&self) -> String {
        self.record.run_id.clone()
    }
    fn now(&self) -> u64 {
        self.deps.clock.now_ms()
    }
    fn log(&self, kind: &str, fields: Value) -> Result<()> {
        self.log
            .lock()
            .map_err(|_| anyhow!("The assistant action log is unavailable."))?
            .record(kind, fields, self.now())?;
        Ok(())
    }
    fn emit(&mut self, event: Event) {
        (self.events)(RunEvent::Agent(event));
    }
    fn state(&mut self, state: RunState, summary: impl Into<String>) {
        let snapshot = RunSnapshot {
            run_id: self.id(),
            actor_id: self.facts.actor_id.clone(),
            state,
            round: self.limiter.totals.rounds,
            elapsed_ms: self.now().saturating_sub(self.start_ms),
            summary: redact::for_output_text(&summary.into()),
            totals: self.limiter.totals.clone(),
        };
        *self
            .control
            .shared
            .status
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = snapshot.clone();
        (self.events)(RunEvent::State(snapshot));
        *self
            .control
            .shared
            .end_fields
            .lock()
            .unwrap_or_else(|error| error.into_inner()) =
            json!({"totals":self.limiter.totals,"leftovers":self.leftovers()});
    }

    fn refresh(&mut self) -> Result<()> {
        let ctx = self.deps.gather(self.host.as_ref())?;
        for activity in &ctx.snapshot.activity {
            if activity.state == ActivityState::Failed
                || activity
                    .actor
                    .as_ref()
                    .is_some_and(|actor| actor.id == self.facts.actor_id)
                || self.humans.activities.get(&activity.id) == Some(&activity_signature(activity))
            {
                continue;
            }
            if let Some(camera) = activity.camera.as_ref() {
                if matches!(activity.operation.as_str(), "forward" | "stop_forward") {
                    self.facts.forwarders.remove(camera);
                    self.owned_forwarders.remove(camera);
                }
                if matches!(activity.operation.as_str(), "auto" | "manual") {
                    self.owned_auto.remove(camera);
                }
            }
        }
        let changes = self
            .humans
            .changes(&ctx, &self.facts.scope, &self.facts.actor_id);
        for (camera, feature, note) in changes {
            self.policy.invalidate_camera(&camera);
            if let Some(feature) = feature {
                if let Some(value) =
                    values(&ctx, &self.facts.scope).get(&(camera.clone(), feature.clone()))
                {
                    self.facts
                        .human_values
                        .insert((camera.clone(), feature), value.clone());
                }
            } else {
                for ((id, feature), value) in values(&ctx, &self.facts.scope)
                    .into_iter()
                    .filter(|((id, _), _)| id == &camera)
                {
                    self.facts.human_values.insert((id, feature), value);
                }
            }
            self.log("human_change", json!({"camera":camera,"label":note}))?;
            self.human_notes.push(note.clone());
            self.pending_human_notes.push(note.clone());
            self.emit(Event::Notice(note));
            match self.request.on_human_change {
                OnHumanChange::Pause => self.control.pause(),
                OnHumanChange::Inform => {}
                OnHumanChange::Stop => self.control.stop(),
            }
        }
        self.humans.accept(&ctx, &self.facts.scope);
        self.facts.forwarders.retain(|camera| {
            ctx.snapshot.cameras.iter().any(|state| {
                state.info.id == *camera
                    && self.facts.worker_pids.get(camera) == Some(&state.worker_pid)
                    && self
                        .owned_forwarders
                        .get(camera)
                        .is_some_and(|forwarding_id| {
                            Some(forwarding_id) == state.forwarding_id.as_ref()
                        })
                    && state.forwarding.is_some()
            })
        });
        self.context = ctx;
        Ok(())
    }

    fn notices(&mut self) {
        for notice in self.limiter.notices(self.now()) {
            self.emit(Event::Notice(notice.message));
        }
    }

    fn boundary(&mut self, needs_answer: bool) -> Result<bool> {
        while (needs_answer
            && self
                .control
                .shared
                .messages
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .is_empty())
            || self.control.shared.pause.load(Ordering::SeqCst)
        {
            if self.control.cancel().load(Ordering::SeqCst) {
                return Ok(false);
            }
            self.limiter.check_wall(self.now())?;
            self.state(
                if needs_answer {
                    RunState::NeedsUser
                } else {
                    RunState::Paused
                },
                if needs_answer {
                    "Needs your answer."
                } else {
                    "Paused. Resume when you are ready."
                },
            );
            self.deps
                .clock
                .wait(Duration::from_millis(250), self.control.cancel());
            self.refresh()?;
        }
        let mut messages: Vec<_> = self
            .control
            .shared
            .messages
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .drain(..)
            .collect();
        if !self.pending_human_notes.is_empty() {
            messages.push(format!(
                "Capturefab observed these camera changes (data): {}",
                serde_json::to_string(&self.pending_human_notes)?
            ));
        }
        if !messages.is_empty() {
            let message = messages.join("\n\n");
            self.conversation.append_user(
                &prompt::user_prefix(&message, &self.context),
                self.model.wire.as_ref(),
            )?;
            self.pending_human_notes.clear();
            self.turn = TurnCache::new();
        }
        Ok(!self.control.cancel().load(Ordering::SeqCst))
    }

    /// A tool round still owes its complete result sequence. Pausing here
    /// must leave queued user messages for the next conversation boundary.
    fn pause_wait(&mut self) -> Result<bool> {
        let mut waited = false;
        while self.control.shared.pause.load(Ordering::SeqCst) {
            waited = true;
            if self.control.cancel().load(Ordering::SeqCst) {
                bail!("The assistant task was stopped.");
            }
            self.limiter.check_wall(self.now())?;
            self.state(RunState::Paused, "Paused. Resume when you are ready.");
            self.deps
                .clock
                .wait(Duration::from_millis(250), self.control.cancel());
            self.refresh()?;
        }
        ensure!(
            !self.control.cancel().load(Ordering::SeqCst),
            "The assistant task was stopped."
        );
        Ok(waited)
    }

    fn run(
        &mut self,
        executor: &mut Executor<'_>,
        approver: Arc<dyn Approver>,
    ) -> Result<(Outcome, String)> {
        let result = self.run_inner(executor, approver);
        if let Err(error) = &result {
            self.conversation.discard_pending();
            if let Some(limit) = error.downcast_ref::<super::limits::LimitReached>() {
                let _ = self.log(
                    "limit",
                    json!({"name":limit.name,"used":limit.used,"max":limit.max}),
                );
                return Ok((
                    Outcome::Limit,
                    format!(
                        "The assistant reached its {} limit. No further actions were started.",
                        limit.name
                    ),
                ));
            }
            if error.downcast_ref::<super::log::LogFull>().is_some() {
                return Ok((
                    Outcome::Limit,
                    "The assistant reached its action log limit. No further actions were started."
                        .into(),
                ));
            }
            if self.control.cancel().load(Ordering::SeqCst) {
                return Ok((Outcome::Stopped, "The assistant task was stopped.".into()));
            }
        }
        result
    }

    fn projection(&self) -> Result<(Value, RequestProjection)> {
        let request = self.model.build(&self.conversation)?;
        let body: Value = serde_json::from_slice(&request.body)?;
        let images: Vec<_> = self
            .checkpoint_images
            .iter()
            .chain(self.conversation.images())
            .cloned()
            .collect();
        let output = if self.conversation.protocol() == Protocol::Generate {
            32768
        } else {
            8192
        };
        let mut projection = super::limits::projection_for_provider(
            &body,
            &images,
            self.model.provider,
            &self.model.model,
            output,
        );
        let actual_images = protocol::wire_image_stats(&body).0;
        projection.input_tokens = projection.input_tokens.saturating_add(
            u64::try_from(actual_images.saturating_sub(images.len()))
                .unwrap_or(u64::MAX)
                .saturating_mul(1600),
        );
        Ok((body, projection))
    }

    fn run_inner(
        &mut self,
        executor: &mut Executor<'_>,
        approver: Arc<dyn Approver>,
    ) -> Result<(Outcome, String)> {
        let mut malformed = 0u32;
        loop {
            if self.control.cancel().load(Ordering::SeqCst) {
                return Ok((Outcome::Stopped, "The assistant task was stopped.".into()));
            }
            self.refresh()?;
            if !self.boundary(false)? {
                return Ok((Outcome::Stopped, "The assistant task was stopped.".into()));
            }
            if let Some(last) = self.last_request_ms {
                let until = last.saturating_add(self.request.limits.min_round_interval_ms);
                while self.now() < until && !self.control.cancel().load(Ordering::SeqCst) {
                    self.limiter.check_wall(self.now())?;
                    self.deps.clock.wait(
                        Duration::from_millis(until.saturating_sub(self.now()).min(250)),
                        self.control.cancel(),
                    );
                    self.refresh()?;
                }
                if !self.boundary(false)? {
                    return Ok((Outcome::Stopped, "The assistant task was stopped.".into()));
                }
            }
            self.limiter.check_totals(self.now())?;
            let budget = protocol::ImageBudget::for_provider(self.model.provider);
            if self.checkpoint_images.len() + self.conversation.images().count() > budget.max_images
            {
                self.rollover(segment::RolloverReason::Images, executor)?;
                continue;
            }
            let (body, projection) = match self.projection() {
                Ok(projected) => projected,
                Err(error)
                    if error
                        .downcast_ref::<super::http::ProviderError>()
                        .is_some_and(|error| {
                            error.kind == super::http::ProviderErrorKind::CutOff
                        }) =>
                {
                    self.rollover(segment::RolloverReason::BodyBytes, executor)?;
                    continue;
                }
                Err(error) => return Err(error),
            };
            let stats = SegmentStats {
                projected_input_tokens: projection.input_tokens,
                context_window_tokens: None,
                images: protocol::wire_image_stats(&body).0,
                rounds: self.segment_rounds,
                body_bytes: serde_json::to_vec(&body)?.len(),
            };
            if let Some(reason) = segment::rollover_reason(
                &self.conversation,
                &stats,
                protocol::ImageBudget::for_provider(self.model.provider),
            )? {
                self.rollover(reason, executor)?;
                continue;
            }
            self.limiter.check_request(self.now(), &projection)?;
            if self.model.needs_credentials_refresh() {
                let settings = self.model.settings.clone();
                let deadline_ms = self.limiter.deadline_ms();
                self.state(RunState::Running, "Refreshing the provider sign-in…");
                let refreshed =
                    credential_work(self.deps, deadline_ms, self.control, move |deps, worker| {
                        ModelSession::refresh_credentials_until(
                            deps,
                            &settings,
                            worker.cancel(),
                            deadline_ms,
                        )
                    });
                self.limiter.check_wall(self.now())?;
                if self.control.cancel().load(Ordering::SeqCst) {
                    return Ok((Outcome::Stopped, "The assistant task was stopped.".into()));
                }
                if let Some(auth) = refreshed? {
                    self.model.auth = auth;
                }
            }
            if self.control.shared.pause.load(Ordering::SeqCst) {
                if !self.boundary(false)? {
                    return Ok((Outcome::Stopped, "The assistant task was stopped.".into()));
                }
                continue;
            }
            for notice in self.limiter.begin_request(self.now(), &projection)? {
                self.emit(Event::Notice(notice.message));
            }
            self.state(RunState::Running, "Thinking…");
            let generation = self.control.shared.generation.load(Ordering::SeqCst);
            let vision_rejected = self.conversation.vision_rejected;
            let started = self.now();
            self.last_request_ms = Some(started);
            let reply = self.model.request_until(
                &mut self.conversation,
                self.deps,
                self.control.cancel(),
                self.limiter.deadline_ms(),
            );
            if generation != self.control.shared.generation.load(Ordering::SeqCst)
                || self.control.cancel().load(Ordering::SeqCst)
            {
                return Ok((Outcome::Stopped, "The assistant task was stopped.".into()));
            }
            let reply = match reply {
                Ok(reply) => reply,
                Err(error) => {
                    self.limiter.check_wall(self.now())?;
                    if !vision_rejected && self.conversation.vision_rejected {
                        self.emit(Event::Notice(
                            "This model can't see images; continuing with measurements only."
                                .into(),
                        ));
                        self.rollover(segment::RolloverReason::Images, executor)?;
                        continue;
                    }
                    let recovery = error
                        .downcast_ref::<super::http::ProviderError>()
                        .map_or(Recovery::None, |error| self.recovery.provider_error(error));
                    match recovery {
                        Recovery::RetryWithoutThinking => {
                            self.conversation =
                                segment::strip_thinking_candidate(&self.conversation)?;
                            self.log("rollover",json!({"from":self.segment,"to":self.segment,"reason":"history_repair","checkpoint_sha256":null}))?;
                            continue;
                        }
                        Recovery::Rollover(reason) => {
                            self.rollover(reason, executor)?;
                            continue;
                        }
                        _ => return Err(error),
                    }
                }
            };
            self.segment_rounds = self.segment_rounds.saturating_add(1);
            self.limiter.record_usage(reply.usage);
            self.log("request",json!({"segment":self.segment,"round":self.limiter.totals.rounds,"ms":self.now().saturating_sub(started),"stop":format!("{:?}",reply.stop).split('(').next().unwrap_or("unknown"),"usage":reply.usage,"cost_usd":self.limiter.totals.cost_usd}))?;
            self.emit(Event::Usage(reply.usage));
            self.limiter.check_totals(self.now())?;
            self.notices();
            self.refresh()?;
            if !reply.text.is_empty() {
                self.last_text = redact::for_model_text(&reply.text);
                self.log("text",json!({"segment":self.segment,"round":self.limiter.totals.rounds,"text":self.last_text.chars().take(4096).collect::<String>()}))?;
                self.emit(Event::Text(self.last_text.clone()));
            }
            match &reply.stop {
                Stop::Malformed(_) => {
                    malformed += 1;
                    if malformed > 1 {
                        bail!(
                            "The model returned an invalid tool call twice. Try again or choose another model."
                        );
                    }
                    self.rollover(segment::RolloverReason::HistoryBinding, executor)?;
                    self.conversation.append_user(&prompt::user_prefix("Your last reply contained an invalid tool call. Return a valid call from the tool list.",&self.context),self.model.wire.as_ref()).or_else(|_| {
                        // Converse permits only one initial user message. The
                        // checkpoint already states the goal and valid tools.
                        if self.conversation.protocol()==Protocol::Converse {Ok(())}else{Err(anyhow!("Could not request a corrected tool call."))}
                    })?;
                    continue;
                }
                Stop::MaxTokens => match self.recovery.stop(&reply.stop) {
                    Recovery::Rollover(reason) => {
                        self.rollover(reason, executor)?;
                        continue;
                    }
                    _ => bail!("The response was cut off; try a shorter request."),
                },
                Stop::Refusal(hint) | Stop::Blocked(hint) => {
                    return Ok((Outcome::NeedsUser, redact::for_model_text(hint)));
                }
                _ => malformed = 0,
            }
            let calls = reply.calls.clone();
            if reply.raw.is_object() || reply.raw.is_array() {
                self.conversation.push_reply(reply.clone())?;
            }
            if calls.is_empty() {
                if matches!(reply.stop, Stop::PauseTurn) {
                    continue;
                }
                if !self.request.wait_for_answer {
                    return Ok((
                        Outcome::NeedsUser,
                        if self.last_text.is_empty() {
                            "The assistant needs your answer.".into()
                        } else {
                            self.last_text.clone()
                        },
                    ));
                }
                self.state(RunState::NeedsUser, "Needs your answer.");
                if !self.boundary(true)? {
                    return Ok((Outcome::Stopped, "The assistant task was stopped.".into()));
                }
                continue;
            }
            let mut results = Vec::new();
            let mut finish = None;
            for call in calls.iter().take(super::conversation::MAX_CALLS) {
                if self.control.cancel().load(Ordering::SeqCst) {
                    self.conversation.discard_pending();
                    return Ok((Outcome::Stopped, "The assistant task was stopped.".into()));
                }
                if finish.is_some() {
                    results.push(ResultRecord::error(
                        call,
                        "The task has finished; this action was not run.",
                    ));
                    continue;
                }
                let (result, done) = self.call(call, executor, approver.clone())?;
                self.limiter.record_result(!result.is_error)?;
                results.push(result);
                if done.is_some() {
                    finish = done;
                }
                if self.policy.needs_user() {
                    finish = Some((
                        Outcome::NeedsUser,
                        "The task needs your help after three declined or unanswered actions."
                            .into(),
                    ));
                }
            }
            self.conversation.append_results(
                results,
                self.model.wire.as_ref(),
                &self.model.vision,
            )?;
            if let Some(done) = finish {
                return Ok(done);
            }
            if self.policy.needs_user() {
                return Ok((
                    Outcome::NeedsUser,
                    "The task needs your help after three declined or unanswered actions.".into(),
                ));
            }
            self.refresh()?;
        }
    }

    fn scoped(&self, call: &ToolCall, executor: &Executor<'_>) -> Result<ToolCall> {
        let mut call = call.clone();
        if call.args.get("camera").is_some() {
            if let Some(camera) = call.args["camera"].as_str() {
                let resolved = tools::resolve_camera(&self.context, "camera", camera, true)
                    .map_err(|issue| anyhow!(issue.message))?;
                let id = executor
                    .aliases
                    .get(resolved.selector())
                    .map(String::as_str)
                    .unwrap_or(resolved.selector());
                ensure!(
                    scope_ids(&self.request.scope, &executor.aliases).contains(id),
                    "{} is not part of this task. Ask the user to add it.",
                    redact::for_model_text(camera)
                );
                call.args["camera"] = json!(id);
            } else if matches!(call.tool.as_str(), "inspect_frame" | "get_camera_features") {
                ensure!(
                    self.request.scope.cameras.len() == 1,
                    "Choose a camera from this task explicitly."
                );
                let camera = &self.request.scope.cameras[0];
                call.args["camera"] = json!(executor.aliases.get(camera).unwrap_or(camera));
            }
        }
        if let Some(destination) = call.args.get("destination") {
            if let Some(name) = destination.as_str() {
                ensure!(
                    self.request
                        .scope
                        .destinations
                        .iter()
                        .any(|allowed| allowed.eq_ignore_ascii_case(name)),
                    "{} is not part of this task. Ask the user to add it.",
                    redact::for_model_text(name)
                );
            } else {
                ensure!(
                    self.request.scope.destinations.is_empty(),
                    "Choose one of this task's allowed destinations."
                );
            }
        }
        if call.tool == "start_recording" {
            ensure!(
                self.request.scope.allow_recordings,
                "Recording is not allowed in this task."
            );
            ensure!(
                call.args["duration"]
                    .as_str()
                    .is_some_and(|duration| !duration.is_empty()),
                "Set a duration for this task's recording."
            );
        }
        if call.tool == "schedule_captures" {
            ensure!(
                self.request.scope.allow_schedules,
                "Schedules are not allowed in this task."
            );
        }
        Ok(call)
    }

    fn checked(&self, id: StepId, call: &ToolCall) -> (Checked, ToolImpact) {
        self.checked_with_secret_preview(id, call, false)
    }
    fn checked_with_secret_preview(
        &self,
        id: StepId,
        call: &ToolCall,
        preview: bool,
    ) -> (Checked, ToolImpact) {
        let mut plan = PlanState::assistant(&self.context);
        plan.actor_id = Some(self.facts.actor_id.clone());
        plan.run = Some(self.facts.clone());
        let checked = tools::check(&self.context, &mut plan, id, call, &|step, field| {
            self.deps.secrets.secret(step, field).is_some()
                || preview
                    && call.tool == "create_s3_destination"
                    && call.args["credentials"] == "keychain"
                    && field == "secret"
        });
        let impact = ToolImpact::from_checked(&checked, plan.planned_bytes, self.now());
        (checked, impact)
    }

    fn call(
        &mut self,
        call: &CallRecord,
        executor: &mut Executor<'_>,
        approver: Arc<dyn Approver>,
    ) -> Result<(ResultRecord, Option<(Outcome, String)>)> {
        self.refresh()?;
        self.pause_wait()?;
        let raw = ToolCall {
            tool: call.name.clone(),
            args: call.args.clone(),
        };
        if call.name == "remember" {
            self.limiter.record_call(&raw)?;
            let notes = call.args["notes"]
                .as_str()
                .filter(|notes| notes.len() <= 2048);
            if call.args.as_object().is_none_or(|args| args.len() != 1) || notes.is_none() {
                return Ok((
                    ResultRecord::error(call, "Use notes containing at most 2,048 bytes."),
                    None,
                ));
            }
            self.notes = vec![redact::for_model_text(notes.unwrap())];
            self.log("tool", json!({"call_id":call.id,"name":call.name,"args":call.args,"outcome":"ok","summary":"Updated task notes."}))?;
            return Ok((
                ResultRecord {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    content: json!({"remembered":true}),
                    is_error: false,
                    images: Vec::new(),
                },
                None,
            ));
        }
        if call.name == "finish_task" {
            self.limiter.record_call(&raw)?;
            let summary = call.args["summary"]
                .as_str()
                .filter(|summary| summary.chars().count() <= 1000);
            let outcome = match call.args["outcome"].as_str() {
                Some("succeeded") => Some(Outcome::Finished),
                Some("failed") => Some(Outcome::Failed),
                Some("needs_user") => Some(Outcome::NeedsUser),
                _ => None,
            };
            if call.args.as_object().is_none_or(|args| args.len() != 2)
                || summary.is_none()
                || outcome.is_none()
            {
                return Ok((
                    ResultRecord::error(
                        call,
                        "Finish with outcome succeeded, failed or needs_user and a summary of at most 1,000 characters.",
                    ),
                    None,
                ));
            }
            let summary = redact::for_model_text(summary.unwrap());
            self.log("tool", json!({"call_id":call.id,"name":call.name,"args":call.args,"outcome":"ok","summary":summary}))?;
            return Ok((
                ResultRecord {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    content: json!({"finished":true}),
                    is_error: false,
                    images: Vec::new(),
                },
                Some((outcome.unwrap(), summary)),
            ));
        }
        let scoped = match self.scoped(&raw, executor) {
            Ok(call) => call,
            Err(error) => {
                self.limiter.record_call(&raw)?;
                self.log("tool",json!({"call_id":call.id,"name":call.name,"args":call.args,"outcome":"refused","summary":error.to_string()}))?;
                return Ok((
                    ResultRecord::error(call, &redact::for_model_text(&error.to_string())),
                    None,
                ));
            }
        };
        let id = StepId(self.next_step);
        self.next_step = self.next_step.saturating_add(1);
        let (mut checked, _) = self.checked(id, &scoped);
        if !checked.ready()
            && checked.call.tool == "create_s3_destination"
            && checked
                .issues
                .iter()
                .filter(|issue| issue.is_error())
                .all(|issue| issue.field.as_deref() == Some("secret"))
        {
            let (preview, _) = self.checked_with_secret_preview(id, &scoped, true);
            if preview.ready() {
                checked = preview;
            }
        }
        self.limiter.record_call(&checked.call)?;
        if !checked.ready() {
            self.log("tool",json!({"call_id":call.id,"name":call.name,"risk":checked.risk,"args":checked.call.args,"outcome":"refused","issues":checked.issues,"summary":checked.summary}))?;
            return Ok((
                ResultRecord {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    content: redact::for_model(
                        json!({"issues":checked.issues,"summary":checked.summary}),
                    ),
                    is_error: true,
                    images: Vec::new(),
                },
                None,
            ));
        }
        let mut approved = None;
        if self.policy.requires(&checked) && self.policy.granted(&checked).is_none() {
            let approval_started = self.now();
            let approval_id = self.next_approval;
            let decision = self.approve(&checked, approver)?;
            let decision = self.policy.record(&checked, &decision);
            let mut approval_record = serde_json::to_value(&decision)?;
            if let Some(record) = approval_record.as_object_mut() {
                record.insert("request_id".into(), json!(approval_id));
                record.insert("tool".into(), json!(checked.call.tool));
                record.insert("risk".into(), json!(checked.risk));
                record.insert("summary".into(), json!(checked.summary));
                record.insert(
                    "waited_ms".into(),
                    json!(self.now().saturating_sub(approval_started)),
                );
            }
            self.log("approval", approval_record)?;
            self.decisions.push(format!(
                "{}: {}",
                checked.call.tool,
                match &decision {
                    ApprovalDecision::Approved { .. } => "approved",
                    ApprovalDecision::Denied { .. } => "denied",
                    ApprovalDecision::TimedOut => "unanswered",
                    ApprovalDecision::Cancelled => "cancelled",
                }
            ));
            match decision {
                ApprovalDecision::Approved { .. } => approved = Some(checked.clone()),
                ApprovalDecision::Cancelled => {
                    if self.control.cancel().load(Ordering::SeqCst) {
                        bail!("The assistant task was stopped.");
                    }
                    return Ok((
                        ResultRecord::error(
                            call,
                            "The action was interrupted. Review the updated camera state before proposing it again.",
                        ),
                        None,
                    ));
                }
                ApprovalDecision::Denied { note, .. } => {
                    return Ok((
                        ResultRecord {
                            id: call.id.clone(),
                            name: call.name.clone(),
                            content: json!({"status":"denied","note":note.map(|note|redact::for_model_text(&note)),"error":super::approval::DENIAL_HINT}),
                            is_error: true,
                            images: Vec::new(),
                        },
                        None,
                    ));
                }
                ApprovalDecision::TimedOut => {
                    return Ok((
                        ResultRecord {
                            id: call.id.clone(),
                            name: call.name.clone(),
                            content: json!({"status":"no_answer","error":super::approval::DENIAL_HINT}),
                            is_error: true,
                            images: Vec::new(),
                        },
                        None,
                    ));
                }
            }
        }
        self.refresh()?;
        if self.pause_wait()? {
            return Ok((
                ResultRecord::error(
                    call,
                    "The task paused while this action was pending. Check the current camera state and propose it again.",
                ),
                None,
            ));
        }
        let (fresh, fresh_impact) = self.checked(id, &checked.call);
        if !fresh.ready()
            || approved
                .as_ref()
                .is_some_and(|old| super::approval::recheck_approved(old, &fresh).is_err())
            || approved.is_none()
                && (fresh.risk > checked.risk
                    || self.policy.requires(&fresh) && self.policy.granted(&fresh).is_none())
        {
            return Ok((
                ResultRecord::error(
                    call,
                    "The action changed while it was being reviewed. Check the current camera state and propose it again.",
                ),
                None,
            ));
        }
        checked = fresh;
        let mut impact = fresh_impact;
        if checked.call.tool == "undo_camera_change" {
            let camera = checked.call.args["camera"].as_str().unwrap_or_default();
            let token = checked.call.args["token"]
                .as_str()
                .map(str::to_owned)
                .or_else(|| {
                    executor
                        .tokens
                        .iter()
                        .rev()
                        .find(|(id, _)| id == camera)
                        .map(|(_, token)| token.clone())
                });
            let count = token.as_deref().and_then(|token| {
                self.context
                    .snapshot
                    .cameras
                    .iter()
                    .find(|state| state.info.id == camera)?
                    .history
                    .entries
                    .iter()
                    .find(|entry| entry.token == token)?
                    .setting_changes
            });
            let Some(count) = count else {
                return Ok((
                    ResultRecord::error(
                        call,
                        "The undo entry has no bounded settings count. Inspect the camera history and ask the user to restore it.",
                    ),
                    None,
                ));
            };
            impact.setting_changes = count;
        }
        if !self.request.scope.effects_may_outlive_run
            && (matches!(
                checked.call.tool.as_str(),
                "start_recording" | "stream_to_url" | "schedule_captures"
            ))
            && (impact.ends_at_ms.is_none()
                || impact
                    .ends_at_ms
                    .is_some_and(|end| end > self.limiter.deadline_ms()))
        {
            return Ok((
                ResultRecord::error(
                    call,
                    "Set an end time within this task's remaining time, or ask the user to allow effects to outlive the task.",
                ),
                None,
            ));
        }
        if checked.call.tool == "inspect_frame"
            && checked.call.args["include_image"].as_bool().unwrap_or(true)
            && self.request.scope.allow_images
            && self.model.vision.enabled
        {
            impact.images = 1;
        }
        self.limiter.check_tool(self.now(), &impact)?;
        self.limiter.record_tool(impact);
        self.notices();
        if tools::spec(&checked.call.tool).is_some_and(|spec| spec.kind == ToolKind::Read) {
            return self.observe(call, &checked);
        }
        if self.control.cancel().load(Ordering::SeqCst) {
            bail!("The assistant task was stopped.");
        }
        self.state(RunState::Running, checked.summary.clone());
        let before = self.context.clone();
        let outcome = executor.run_step(&checked, self.control.cancel());
        match outcome {
            Ok(outcome) => {
                self.track(&checked, &before, &outcome, executor)?;
                self.log("tool",json!({"call_id":call.id,"name":call.name,"risk":checked.risk,"args":checked.call.args,"outcome":"ok","summary":outcome.summary,"commands":outcome.results}))?;
                if self.context.snapshot.dataset.is_some() {
                    let mut context = crate::context::Context::new();
                    context.insert("run".into(), self.id());
                    if let Err(error) = self.host.execute(
                        None,
                        SessionCommand::DatasetNote {
                            text: format!("Assistant: {}", checked.summary),
                            context,
                            camera: checked.call.args["camera"].as_str().map(str::to_owned),
                        },
                    ) {
                        self.emit(Event::Notice(format!("The camera action finished, but its dataset note could not be written: {}",redact::for_output_text(&error.to_string()))));
                    }
                }
                let output = redact::for_model(serde_json::to_value(&outcome)?);
                self.emit(Event::ToolRan {
                    call: checked.call.clone(),
                    output: output.clone(),
                    images: Vec::new(),
                });
                Ok((
                    ResultRecord {
                        id: call.id.clone(),
                        name: call.name.clone(),
                        content: output,
                        is_error: false,
                        images: Vec::new(),
                    },
                    None,
                ))
            }
            Err(error) => {
                let acknowledged = executor.acknowledged().to_vec();
                let changed_settings = acknowledged.iter().any(|reply| {
                    reply
                        .get("camera_change")
                        .is_some_and(|value| !value.is_null())
                });
                if acknowledged.iter().any(|reply| {
                    reply
                        .get("camera_change")
                        .is_some_and(|change| !change.is_null())
                        || reply["forwarding_id"]
                            .as_str()
                            .is_some_and(crate::session::valid_forwarding_id)
                }) {
                    self.track(
                        &checked,
                        &before,
                        &tools::StepOutcome {
                            summary: checked.summary.clone(),
                            results: acknowledged,
                            watch: Vec::new(),
                        },
                        executor,
                    )?;
                    self.emit(Event::Notice(if changed_settings {"Some camera settings changed before the action failed. They are included in this task's restore proposal."}else{"The worker accepted the recording or stream before verification failed. Review this task's remaining effects."}.into()));
                } else {
                    self.refresh()?;
                }
                self.log("tool",json!({"call_id":call.id,"name":call.name,"risk":checked.risk,"args":checked.call.args,"outcome":"error","summary":redact::for_output_error(&error.to_string(),&checked.call.args)}))?;
                Ok((
                    ResultRecord::error(
                        call,
                        &redact::for_model_text(&redact::for_output_error(
                            &error.to_string(),
                            &checked.call.args,
                        )),
                    ),
                    None,
                ))
            }
        }
    }

    fn approve(
        &mut self,
        checked: &Checked,
        approver: Arc<dyn Approver>,
    ) -> Result<ApprovalDecision> {
        let timeout = self
            .request
            .limits
            .approval_timeout_ms
            .min(self.limiter.remaining_wall_ms(self.now()));
        self.limiter.check_wall(self.now())?;
        let expires = self.now().saturating_add(timeout);
        let request = ApprovalRequest::new(self.next_approval, &self.id(), checked, expires);
        self.next_approval = self.next_approval.saturating_add(1);
        self.state(RunState::WaitingApproval, checked.summary.clone());
        (self.events)(RunEvent::Approval(request.clone()));
        let local_cancel = Arc::new(AtomicBool::new(false));
        struct CancelOnExit(Arc<AtomicBool>);
        impl Drop for CancelOnExit {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let _cancel_on_exit = CancelOnExit(local_cancel.clone());
        let worker_cancel = local_cancel.clone();
        let (sender, receiver) = mpsc::sync_channel(1);
        thread::Builder::new()
            .name("capturefab-assistant-approval".into())
            .spawn(move || {
                let decision =
                    approver.request(request, &worker_cancel, Duration::from_millis(timeout));
                let _ = sender.send(decision);
            })?;
        loop {
            if self.control.cancel().load(Ordering::SeqCst) {
                local_cancel.store(true, Ordering::SeqCst);
                return Ok(ApprovalDecision::Cancelled);
            }
            if let Err(error) = self.limiter.check_wall(self.now()) {
                local_cancel.store(true, Ordering::SeqCst);
                return Err(error.into());
            }
            self.refresh()?;
            if self.control.shared.pause.load(Ordering::SeqCst) {
                local_cancel.store(true, Ordering::SeqCst);
                return Ok(ApprovalDecision::Cancelled);
            }
            if self.now() >= expires {
                local_cancel.store(true, Ordering::SeqCst);
                return Ok(ApprovalDecision::TimedOut);
            }
            match receiver.try_recv() {
                Ok(decision) => return Ok(decision),
                Err(mpsc::TryRecvError::Disconnected) => return Ok(ApprovalDecision::TimedOut),
                Err(mpsc::TryRecvError::Empty) => {}
            }
            // Yield also gives immediate scripted approvers a chance to send
            // their answer when tests use a clock that advances without sleep.
            thread::yield_now();
            self.deps
                .clock
                .wait(Duration::from_millis(250), self.control.cancel());
        }
    }

    fn observe(
        &mut self,
        call: &CallRecord,
        checked: &Checked,
    ) -> Result<(ResultRecord, Option<(Outcome, String)>)> {
        if checked.call.tool == "wait_for" {
            let condition = match checked.call.args["until"].as_str() {
                Some("time") => "the chosen delay",
                Some("frames") => "new camera frames",
                Some("auto_settled") => "auto exposure to settle",
                Some("job_done") => "the capture schedule to finish",
                Some("recording_stopped") => "the recording to stop",
                Some("activity") => "session activity",
                _ => "the checked condition",
            };
            let timeout = checked.call.args["timeout"]
                .as_str()
                .unwrap_or("the checked timeout");
            let camera = checked.call.args["camera"]
                .as_str()
                .map(|camera| format!(" on {camera}"))
                .unwrap_or_default();
            self.state(
                RunState::Waiting,
                format!("Waiting up to {timeout} for {condition}{camera}."),
            );
        } else {
            self.state(RunState::Running, checked.summary.clone());
        }
        let baseline = self.context.clone();
        let scope = self.facts.scope.clone();
        let actor = self.facts.actor_id.clone();
        let tracker = &self.humans;
        let interrupt = || {
            if self.now() >= self.limiter.deadline_ms() {
                return Some("The task reached its time limit.".into());
            }
            if self.control.shared.pause.load(Ordering::SeqCst) {
                return Some("The task is paused.".into());
            }
            self.host.snapshot().ok().and_then(|snapshot| {
                let mut ctx = baseline.clone();
                ctx.snapshot = snapshot;
                tracker
                    .changes(&ctx, &scope, &actor)
                    .first()
                    .map(|(_, _, note)| note.clone())
            })
        };
        let mut options = ReadOptions::new(Origin::Assistant, self.control.cancel());
        options.allow_images = self.request.scope.allow_images && self.model.vision.enabled;
        options.max_wait = Duration::from_millis(self.limiter.remaining_wall_ms(self.now()));
        options.turn = Some(&self.turn);
        options.interrupt = Some(&interrupt);
        options.image_number = self.limiter.totals.images.max(1);
        let output =
            tools::read::run_read(self.host.as_ref(), &self.context, &checked.call, &options);
        self.limiter.check_wall(self.now())?;
        match output {
            Ok(output) => {
                let images: Vec<_> = output
                    .images
                    .into_iter()
                    .map(|image| ImagePart {
                        mime: image.mime,
                        data: image.data,
                        width: image.width,
                        height: image.height,
                        label: image.label,
                    })
                    .collect();
                self.log("tool",json!({"call_id":call.id,"name":call.name,"risk":Risk::Observe,"args":checked.call.args,"outcome":"ok","summary":checked.summary}))?;
                if checked.call.tool == "inspect_frame" {
                    self.log("observation",json!({"camera":checked.call.args["camera"],"metrics":output.value,"sent":images.iter().map(|image|json!({"w":image.width,"h":image.height,"bytes":image.data.len(),"sha256":format!("{:x}",sha2::Sha256::digest(&image.data))})).collect::<Vec<_>>()}))?;
                }
                for image in &images {
                    self.emit(Event::Image(image.clone()));
                }
                self.emit(Event::ToolRan {
                    call: checked.call.clone(),
                    output: output.value.clone(),
                    images: images.clone(),
                });
                self.refresh()?;
                Ok((
                    ResultRecord {
                        id: call.id.clone(),
                        name: call.name.clone(),
                        content: output.value,
                        is_error: false,
                        images,
                    },
                    None,
                ))
            }
            Err(issues) => {
                self.log("tool",json!({"call_id":call.id,"name":call.name,"risk":Risk::Observe,"args":checked.call.args,"outcome":"error","issues":issues,"summary":checked.summary}))?;
                self.refresh()?;
                Ok((
                    ResultRecord {
                        id: call.id.clone(),
                        name: call.name.clone(),
                        content: json!({"issues":issues}),
                        is_error: true,
                        images: Vec::new(),
                    },
                    None,
                ))
            }
        }
    }

    fn track(
        &mut self,
        checked: &Checked,
        before: &Context,
        outcome: &tools::StepOutcome,
        executor: &mut Executor<'_>,
    ) -> Result<()> {
        fn acknowledgements(
            value: &Value,
            forwarding_id: &mut Option<String>,
            jobs: &mut BTreeSet<u64>,
        ) {
            if let Some(id) = value["forwarding_id"].as_str() {
                *forwarding_id = Some(id.to_owned());
            }
            if let Some(id) = value["job"]["id"].as_u64() {
                jobs.insert(id);
            }
            match value {
                Value::Array(values) => {
                    for value in values {
                        acknowledgements(value, forwarding_id, jobs);
                    }
                }
                Value::Object(values) => {
                    for value in values.values() {
                        acknowledgements(value, forwarding_id, jobs);
                    }
                }
                _ => {}
            }
        }
        let mut acknowledged_forwarding_id = None;
        let mut acknowledged_jobs = BTreeSet::new();
        for result in &outcome.results {
            acknowledgements(
                result,
                &mut acknowledged_forwarding_id,
                &mut acknowledged_jobs,
            );
        }
        self.facts.scope = scope_ids(&self.request.scope, &executor.aliases);
        let camera = checked.call.args["camera"].as_str().map(|camera| {
            executor
                .aliases
                .get(camera)
                .cloned()
                .unwrap_or_else(|| camera.into())
        });
        if matches!(
            checked.call.tool.as_str(),
            "start_recording" | "stream_to_url"
        ) && let (Some(camera), Some(id)) = (
            &camera,
            acknowledged_forwarding_id
                .as_ref()
                .filter(|id| crate::session::valid_forwarding_id(id)),
        ) {
            self.record.forwarders.insert(camera.clone(), id.clone());
            self.uncertain_forwarders.insert(camera.clone());
            if let Some(state) = before
                .snapshot
                .cameras
                .iter()
                .find(|state| state.info.id == *camera)
            {
                self.record
                    .worker_pids
                    .entry(camera.clone())
                    .or_insert(state.worker_pid);
            }
        }
        let fresh = self.deps.gather(self.host.as_ref()).or_else(|error| {
            let mut fallback = before.clone();
            fallback.snapshot = self.host.snapshot().map_err(|_| error)?;
            Ok::<Context, anyhow::Error>(fallback)
        })?;
        self.settle_forwarders(&fresh);
        if let Some(camera) = &camera {
            if let Some(state) = fresh
                .snapshot
                .cameras
                .iter()
                .find(|state| state.info.id == *camera)
            {
                if checked.call.tool == "connect_camera"
                    || !self.facts.worker_pids.contains_key(camera)
                {
                    if self
                        .facts
                        .worker_pids
                        .get(camera)
                        .is_some_and(|pid| *pid != state.worker_pid)
                    {
                        forget_worker(
                            camera,
                            &mut self.facts,
                            &mut self.record,
                            &mut executor.tokens,
                            &mut self.policy,
                            &mut self.owned_forwarders,
                            &mut self.owned_auto,
                        );
                    }
                    self.facts
                        .worker_pids
                        .insert(camera.clone(), state.worker_pid);
                    self.record
                        .worker_pids
                        .insert(camera.clone(), state.worker_pid);
                    self.humans.workers.insert(camera.clone(), state.worker_pid);
                }
                if matches!(
                    checked.call.tool.as_str(),
                    "start_recording" | "stream_to_url"
                ) && let Some(id) = acknowledged_forwarding_id
                    .as_ref()
                    .filter(|id| !state.stale && Some(*id) == state.forwarding_id.as_ref())
                {
                    self.facts.forwarders.insert(camera.clone());
                    self.owned_forwarders.insert(camera.clone(), id.clone());
                    self.record.forwarders.insert(camera.clone(), id.clone());
                }
                if checked.call.tool == "schedule_captures" {
                    for job in &state.jobs {
                        if acknowledged_jobs.contains(&job.id) {
                            self.facts.jobs.insert((camera.clone(), job.id));
                        }
                    }
                }
                if checked.call.tool == "set_auto_exposure" {
                    if state.auto.as_ref().is_some_and(|auto| auto.state != "off") {
                        self.owned_auto.insert(camera.clone());
                    } else {
                        self.owned_auto.remove(camera);
                    }
                }
            }
            let mut after = BTreeMap::new();
            fn read_after(value: &Value, after: &mut BTreeMap<String, String>) {
                if let Some(values) = value.get("after").and_then(Value::as_array) {
                    for value in values {
                        if let (Some(name), Some(value)) =
                            (value["name"].as_str(), value.get("value"))
                        {
                            after.insert(name.into(), value_text(value));
                        }
                    }
                }
                match value {
                    Value::Array(values) => {
                        for value in values {
                            read_after(value, after);
                        }
                    }
                    Value::Object(values) => {
                        for (key, value) in values {
                            if key != "before" && key != "after" {
                                read_after(value, after);
                            }
                        }
                    }
                    _ => {}
                }
            }
            for result in &outcome.results {
                read_after(result, &mut after);
            }
            if matches!(
                checked.call.tool.as_str(),
                "set_auto_exposure" | "undo_camera_change"
            ) || checked.call.tool == "run_camera_command"
                && checked.call.args["command"]
                    .as_str()
                    .is_some_and(|command| command.ends_with("AutoOnce"))
            {
                for ((id, feature), value) in values(&fresh, &self.facts.scope)
                    .into_iter()
                    .filter(|((id, _), _)| id == camera)
                {
                    if values(before, &self.facts.scope).get(&(id, feature.clone())) != Some(&value)
                    {
                        after.insert(feature, value);
                    }
                }
            }
            let banked = checked.call.args["values"]
                .as_array()
                .is_some_and(|values| {
                    values.iter().any(|value| {
                        value["feature"]
                            .as_str()
                            .is_some_and(|feature| feature.ends_with("Selector"))
                    })
                });
            let old_values = values(before, &self.facts.scope);
            for (feature, last) in after {
                let key = (camera.clone(), feature.clone());
                if let Some(previous) = old_values.get(&key) {
                    if let Some(row) = self
                        .record
                        .settings
                        .iter_mut()
                        .find(|row| row.camera == *camera && row.feature == feature)
                    {
                        row.last = last.clone();
                        row.needs_review |= banked && !feature.ends_with("Selector");
                    } else {
                        self.record.settings.push(SettingChange {
                            camera: camera.clone(),
                            feature: feature.clone(),
                            before: previous.clone(),
                            last: last.clone(),
                            needs_review: banked && !feature.ends_with("Selector"),
                        });
                    }
                    self.experiments.push(format!(
                        "{} {feature}: {previous} → {last}",
                        redact::for_model_text(camera)
                    ));
                }
                self.humans.values.insert(key, last);
            }
        }
        self.facts.tokens = executor.tokens.clone();
        (self.events)(RunEvent::Changes(self.record.settings.clone()));
        self.refresh()?;
        self.state(RunState::Running, checked.summary.clone());
        Ok(())
    }

    fn rollover(
        &mut self,
        reason: segment::RolloverReason,
        executor: &mut Executor<'_>,
    ) -> Result<()> {
        self.conversation.check_invariant()?;
        self.refresh()?;
        self.limiter.check_wall(self.now())?;
        let mut images = Vec::new();
        let budget = protocol::ImageBudget::for_provider(self.model.provider);
        let mut encoded_bytes = 0usize;
        if self.request.scope.allow_images
            && self.model.vision.enabled
            && self.model.provider != ProviderId::Perplexity
        {
            'cameras: for camera in self
                .facts
                .scope
                .iter()
                .filter(|camera| {
                    self.context
                        .snapshot
                        .cameras
                        .iter()
                        .any(|state| state.info.id == **camera)
                })
                .cloned()
                .collect::<Vec<_>>()
                .into_iter()
                .take(budget.max_images.saturating_sub(1))
            {
                if self.control.cancel().load(Ordering::SeqCst) {
                    bail!("The assistant task was stopped.");
                }
                let impact = ToolImpact {
                    images: 1,
                    ..ToolImpact::default()
                };
                self.limiter.check_tool(self.now(), &impact)?;
                self.limiter.record_tool(impact);
                let call = ToolCall {
                    tool: "inspect_frame".into(),
                    args: json!({"camera":camera,"fresh":0,"max_dimension":1280,"region":null,"include_image":true}),
                };
                let mut options = ReadOptions::new(Origin::Assistant, self.control.cancel());
                options.image_number = self.limiter.totals.images;
                options.max_wait =
                    Duration::from_millis(self.limiter.remaining_wall_ms(self.now()));
                if let Ok(output) =
                    tools::read::run_read(self.host.as_ref(), &self.context, &call, &options)
                {
                    for image in output.images {
                        let bytes = image.data.len().div_ceil(3).saturating_mul(4);
                        if encoded_bytes.saturating_add(bytes)
                            > budget.max_bytes.min(4 * 1024 * 1024)
                        {
                            break 'cameras;
                        }
                        encoded_bytes = encoded_bytes.saturating_add(bytes);
                        let image = ImagePart {
                            mime: image.mime,
                            data: image.data,
                            width: image.width,
                            height: image.height,
                            label: image.label,
                        };
                        self.emit(Event::Image(image.clone()));
                        images.push(image);
                    }
                }
            }
        }
        let checkpoint = Checkpoint {
            goal: self.request.goal.clone(),
            mode: format!("{:?}", self.request.mode),
            scope: serde_json::to_value(&self.request.scope)?,
            limits_remaining: serde_json::to_value(self.limiter.remaining(self.now()))?,
            cameras: self
                .context
                .snapshot
                .cameras
                .iter()
                .filter(|camera| self.facts.scope.contains(&camera.info.id))
                .map(|camera| {
                    redact::for_model(serde_json::to_value(camera).unwrap_or(Value::Null))
                })
                .collect(),
            experiments: self.experiments.clone(),
            user_decisions: self.decisions.clone(),
            human_changes: self.human_notes.clone(),
            notes: self.notes.clone(),
            last_message: self.last_text.clone(),
        };
        let rendered = checkpoint.render(
            self.segment.saturating_add(1),
            &self.context.zone.rfc3339(self.now()),
        )?;
        let previous = self.segment;
        self.conversation = segment::rollover(&self.conversation, &rendered, &images)?;
        self.pending_human_notes.clear();
        self.checkpoint_images = images;
        self.segment = self.segment.saturating_add(1);
        self.segment_rounds = 0;
        self.log("rollover",json!({"from":previous,"to":self.segment,"reason":reason,"checkpoint_sha256":rendered.sha256}))?;
        self.emit(Event::Notice(format!(
            "Continuing in segment {} with a fresh camera checkpoint.",
            self.segment
        )));
        self.facts.scope = scope_ids(&self.request.scope, &executor.aliases);
        Ok(())
    }

    fn settle_forwarders(&mut self, ctx: &Context) {
        for camera in self.uncertain_forwarders.clone() {
            if let Some(state) = ctx
                .snapshot
                .cameras
                .iter()
                .find(|state| state.info.id == camera && !state.stale)
            {
                if self.record.worker_pids.get(&camera) != Some(&state.worker_pid) {
                    self.uncertain_forwarders.remove(&camera);
                    continue;
                }
                if let Some(id) = self.record.forwarders.get(&camera).filter(|id| {
                    Some(*id) == state.forwarding_id.as_ref() && state.forwarding.is_some()
                }) {
                    self.owned_forwarders.insert(camera.clone(), id.clone());
                    self.facts.forwarders.insert(camera.clone());
                    self.facts
                        .worker_pids
                        .insert(camera.clone(), state.worker_pid);
                    self.record
                        .worker_pids
                        .insert(camera.clone(), state.worker_pid);
                }
                self.uncertain_forwarders.remove(&camera);
            }
        }
    }

    fn leftovers(&self) -> Vec<Leftover> {
        let mut leftovers = Vec::new();
        for camera in self.context.snapshot.cameras.iter().filter(|camera| {
            self.facts.worker_pids.get(&camera.info.id) == Some(&camera.worker_pid)
        }) {
            if self.facts.forwarders.contains(&camera.info.id)
                && camera.forwarding.is_some()
                && self
                    .owned_forwarders
                    .get(&camera.info.id)
                    .is_some_and(|forwarding_id| {
                        Some(forwarding_id) == camera.forwarding_id.as_ref()
                    })
            {
                leftovers.push(Leftover {
                    kind: "recording".into(),
                    camera: camera.info.id.clone(),
                    summary: format!(
                        "Recording or streaming on {}.",
                        redact::for_output_text(&camera.info.model)
                    ),
                    job_id: None,
                    until_ms: camera.forwarding_until_ms,
                });
            }
            for job in camera.jobs.iter().filter(|job| {
                job.active() && self.facts.jobs.contains(&(camera.info.id.clone(), job.id))
            }) {
                leftovers.push(Leftover {
                    kind: "schedule".into(),
                    camera: camera.info.id.clone(),
                    summary: format!(
                        "Schedule #{} on {}.",
                        job.id,
                        redact::for_output_text(&camera.info.model)
                    ),
                    job_id: Some(job.id),
                    until_ms: None,
                });
            }
            if camera.auto.as_ref().is_some_and(|auto| auto.state != "off")
                && self.owned_auto.contains(&camera.info.id)
            {
                leftovers.push(Leftover {
                    kind: "auto".into(),
                    camera: camera.info.id.clone(),
                    summary: format!(
                        "Auto exposure remains on for {}.",
                        redact::for_output_text(&camera.info.model)
                    ),
                    job_id: None,
                    until_ms: None,
                });
            }
            let changed = self
                .record
                .settings
                .iter()
                .filter(|setting| {
                    setting.camera == camera.info.id && setting.before != setting.last
                })
                .count();
            if changed > 0 {
                leftovers.push(Leftover {
                    kind: "settings".into(),
                    camera: camera.info.id.clone(),
                    summary: format!(
                        "Changed {changed} settings on {}.",
                        redact::for_output_text(&camera.info.model)
                    ),
                    job_id: None,
                    until_ms: None,
                });
            }
        }
        for camera in &self.uncertain_forwarders {
            leftovers.push(Leftover {kind:"recording".into(),camera:camera.clone(),summary:format!("A recording or stream was accepted on {}. Its current state is unavailable; review the camera before starting more work.",redact::for_output_text(camera)),job_id:None,until_ms:None});
        }
        leftovers
    }

    fn finish(&mut self, outcome: Outcome, summary: String) -> Result<RunOutcome> {
        if let Ok(ctx) = self.deps.gather(self.host.as_ref()) {
            self.settle_forwarders(&ctx);
            self.context = ctx;
        }
        let summary = redact::for_output_text(&summary);
        let report = RunOutcome {
            run_id: self.id(),
            outcome,
            summary: summary.clone(),
            totals: self.limiter.totals.clone(),
            leftovers: self.leftovers(),
            log: self
                .log
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .path()
                .to_owned(),
            restore: self.record.clone(),
        };
        if !self.control.shared.ended.swap(true, Ordering::SeqCst) {
            self.log.lock().map_err(|_|anyhow!("The assistant action log is unavailable."))?.finish(json!({"outcome":outcome,"summary":summary,"totals":report.totals,"leftovers":report.leftovers}),self.now())?;
        }
        self.state(
            match outcome {
                Outcome::Finished => RunState::Finished,
                Outcome::NeedsUser => RunState::NeedsUser,
                Outcome::Stopped | Outcome::AppQuit => RunState::Stopped,
                Outcome::Limit => RunState::LimitReached,
                Outcome::Failed => RunState::Failed,
            },
            summary,
        );
        (self.events)(RunEvent::Finished(report.clone()));
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        assistant::{
            agent::{
                Clock, ContextSource,
                tests::{Script, deps, end_reply, tool_reply},
            },
            approval::{ApprovalScope, DenyApprover},
            http::{Http, HttpRequest, HttpResponse, ProviderError, Transport},
        },
        tools::tests::{NOW, fixture_context, fixture_snapshot},
    };
    use std::sync::atomic::AtomicUsize;

    type SentCommand = (Option<SessionActor>, Option<String>, SessionCommand);
    #[derive(Clone)]
    struct Host {
        snapshot: Arc<Mutex<SessionSnapshot>>,
        sent: Arc<Mutex<Vec<SentCommand>>>,
        actor: Option<SessionActor>,
        fail_start: Arc<AtomicBool>,
    }
    impl Host {
        fn new() -> Self {
            let mut snapshot = fixture_snapshot();
            snapshot
                .cameras
                .retain(|camera| camera.info.id == "sim:0" || camera.info.id == "sim:1");
            snapshot.activity.clear();
            snapshot.activity_revision = 0;
            snapshot.dataset = None;
            for camera in &mut snapshot.cameras {
                camera.auto = None;
                camera.jobs.clear();
                camera.forwarding = None;
                camera.forwarding_since_ms = None;
                camera.forwarding_until_ms = None;
                camera.streaming = false;
                for feature in &mut camera.features {
                    if feature.name == "Gain" {
                        feature.value = Some(json!(0));
                    }
                    if feature.name.ends_with("Auto") {
                        feature.value = Some(json!("Off"));
                    }
                }
            }
            Self {
                snapshot: Arc::new(Mutex::new(snapshot)),
                sent: Arc::new(Mutex::new(Vec::new())),
                actor: None,
                fail_start: Arc::new(AtomicBool::new(false)),
            }
        }
        fn gain(&self, value: u32) {
            let mut snapshot = self.snapshot.lock().unwrap();
            let feature = snapshot
                .cameras
                .iter_mut()
                .find(|camera| camera.info.id == "sim:0")
                .unwrap()
                .features
                .iter_mut()
                .find(|feature| feature.name == "Gain")
                .unwrap();
            feature.value = Some(json!(value));
        }
        fn count(&self, name: &str) -> usize {
            self.sent
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, _, command)| serde_json::to_value(command).unwrap()["op"] == name)
                .count()
        }
    }
    impl ToolHost for Host {
        fn snapshot(&self) -> Result<SessionSnapshot> {
            Ok(self.snapshot.lock().unwrap().clone())
        }
        fn has_session(&self) -> bool {
            true
        }
        fn actor_id(&self) -> Option<String> {
            self.actor.as_ref().map(|actor| actor.id.clone())
        }
        fn with_actor(&self, actor: SessionActor) -> Result<Arc<dyn ToolHost>> {
            let mut host = self.clone();
            host.actor = Some(actor);
            Ok(Arc::new(host))
        }
        fn execute(&self, camera: Option<&str>, command: SessionCommand) -> Result<Value> {
            self.sent.lock().unwrap().push((
                self.actor.clone(),
                camera.map(str::to_owned),
                command.clone(),
            ));
            let mut snapshot = self.snapshot.lock().unwrap();
            match command {
                SessionCommand::SetMany { values } => {
                    let state = snapshot
                        .cameras
                        .iter_mut()
                        .find(|state| Some(state.info.id.as_str()) == camera)
                        .unwrap();
                    let mut result = Vec::new();
                    for value in values {
                        let parsed = serde_json::from_str::<Value>(&value.value)
                            .unwrap_or_else(|_| json!(value.value));
                        state
                            .features
                            .iter_mut()
                            .find(|feature| feature.name == value.feature)
                            .unwrap()
                            .value = Some(parsed.clone());
                        result.push(json!({"name":value.feature,"value":parsed}));
                    }
                    Ok(
                        json!({"values":result,"camera_change":{"token":format!("change:{}",self.count("set_many"))}}),
                    )
                }
                SessionCommand::Start | SessionCommand::Stop => {
                    if matches!(command, SessionCommand::Start)
                        && self.fail_start.load(Ordering::SeqCst)
                    {
                        bail!("The fixture could not restart live view.");
                    }
                    let state = snapshot
                        .cameras
                        .iter_mut()
                        .find(|state| Some(state.info.id.as_str()) == camera)
                        .unwrap();
                    state.streaming = matches!(command, SessionCommand::Start);
                    Ok(json!({"streaming":state.streaming}))
                }
                SessionCommand::Status => Ok(serde_json::to_value(&*snapshot)?),
                SessionCommand::Execute { feature } if feature == "GainAutoOnce" => {
                    let state = snapshot
                        .cameras
                        .iter_mut()
                        .find(|state| Some(state.info.id.as_str()) == camera)
                        .unwrap();
                    state
                        .features
                        .iter_mut()
                        .find(|feature| feature.name == "Gain")
                        .unwrap()
                        .value = Some(json!(2));
                    Ok(json!({"executed":feature}))
                }
                _ => Ok(json!({})),
            }
        }
    }
    struct Contexts {
        root: PathBuf,
    }
    impl ContextSource for Contexts {
        fn gather(&self, host: &dyn ToolHost) -> Result<Context> {
            let mut ctx = fixture_context(host.snapshot()?);
            ctx.capture_root = self.root.clone();
            Ok(ctx)
        }
    }
    struct ClockStub(AtomicU64);
    impl Clock for ClockStub {
        fn now_ms(&self) -> u64 {
            self.0.load(Ordering::SeqCst)
        }
        fn wait(&self, duration: Duration, cancel: &AtomicBool) -> bool {
            self.0
                .fetch_add(duration.as_millis().try_into().unwrap(), Ordering::SeqCst);
            // Give scripted approver workers a deterministic scheduling point.
            thread::sleep(Duration::from_micros(100));
            !cancel.load(Ordering::SeqCst)
        }
    }
    fn fixture(replies: Vec<Value>) -> (Deps, Arc<Script>, Host) {
        let (mut deps, script, _) = deps(ProviderId::Anthropic, replies);
        let host = Host::new();
        let root = deps.log_directory.join("captures");
        std::fs::create_dir(&root).unwrap();
        deps.host = Arc::new(host.clone());
        deps.context = Arc::new(Contexts { root });
        deps.clock = Arc::new(ClockStub(AtomicU64::new(NOW)));
        (deps, script, host)
    }
    fn request() -> RunRequest {
        RunRequest {
            goal: "Set Gain to two and verify the result.".into(),
            mode: RunMode::Autonomous,
            scope: RunScope {
                cameras: vec!["sim:0".into()],
                ..RunScope::default()
            },
            limits: Limits::default(),
            on_human_change: OnHumanChange::Inform,
            provider: None,
            model: None,
            wait_for_answer: false,
        }
    }
    fn set_args(value: u32) -> Value {
        json!({"camera":"sim:0","values":[{"feature":"Gain","value":value.to_string(),"expect":null}],"restart_stream":false})
    }
    fn reply(id: &str, name: &str, args: Value) -> Value {
        tool_reply(ProviderId::Anthropic, id, name, args)
    }
    fn finished() -> Value {
        reply(
            "finish",
            "finish_task",
            json!({"outcome":"succeeded","summary":"Verified the camera."}),
        )
    }
    fn batch(calls: Vec<(&str, &str, Value)>) -> Value {
        json!({"content":calls.into_iter().map(|(id,name,args)|json!({"type":"tool_use","id":id,"name":name,"input":args})).collect::<Vec<_>>(),"stop_reason":"tool_use","usage":{"input_tokens":100,"output_tokens":10}})
    }
    fn run(deps: &Deps, req: RunRequest, approver: Arc<dyn Approver>) -> RunOutcome {
        run_blocking(req, deps, approver, &RunControl::default(), &mut |_| {}).unwrap()
    }
    struct Approvals {
        count: AtomicUsize,
        scope: ApprovalScope,
        host: Option<Host>,
    }
    impl Approvals {
        fn new(scope: ApprovalScope) -> Self {
            Self {
                count: AtomicUsize::new(0),
                scope,
                host: None,
            }
        }
    }
    impl Approver for Approvals {
        fn request(&self, _: ApprovalRequest, _: &AtomicBool, _: Duration) -> ApprovalDecision {
            self.count.fetch_add(1, Ordering::SeqCst);
            if let Some(host) = &self.host {
                host.gain(7);
            }
            ApprovalDecision::Approved {
                scope: self.scope,
                by: SessionActor::new("desktop:test", "Desktop", ActorKind::Human).unwrap(),
            }
        }
    }
    fn cleanup(deps: Deps) {
        let path = deps.log_directory.clone();
        drop(deps);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn autonomous_writes_are_scoped_attributed_and_restorable() {
        let (deps, _, host) = fixture(vec![
            reply("set", "set_camera_features", set_args(2)),
            finished(),
        ]);
        let report = run(&deps, request(), Arc::new(DenyApprover::default()));
        assert_eq!(report.outcome, Outcome::Finished);
        assert_eq!(report.totals.setting_changes, 1);
        assert_eq!(host.count("set_many"), 1);
        let sent = host.sent.lock().unwrap();
        let (actor, camera, _) = sent
            .iter()
            .find(|(_, _, command)| matches!(command, SessionCommand::SetMany { .. }))
            .unwrap();
        assert_eq!(camera.as_deref(), Some("sim:0"));
        assert_eq!(actor.as_ref().unwrap().kind, ActorKind::Agent);
        assert!(actor.as_ref().unwrap().id.starts_with("assistant:"));
        drop(sent);
        let restore = restore(&report.restore, &deps.gather(deps.host.as_ref()).unwrap());
        assert_eq!(restore.calls.len(), 1);
        assert_eq!(
            restore.calls[0].args["values"][0],
            json!({"feature":"Gain","value":"0","expect":"2"})
        );
        assert!(
            report
                .leftovers
                .iter()
                .any(|leftover| leftover.kind == "settings")
        );
        let events = super::super::log::read_log_in(&deps.log_directory, &report.run_id).unwrap();
        assert_eq!(events.first().unwrap()["kind"], "start");
        assert_eq!(events.last().unwrap()["kind"], "end");
        assert!(
            !serde_json::to_string(&events)
                .unwrap()
                .contains("fixture-auth-never-in-body")
        );
        cleanup(deps);
    }

    #[test]
    fn supervised_once_and_for_run_grants_obey_the_policy() {
        for (scope, expected) in [(ApprovalScope::Once, 2), (ApprovalScope::ForRun, 1)] {
            let (deps, _, host) = fixture(vec![
                reply("a", "set_camera_features", set_args(2)),
                reply("b", "set_camera_features", set_args(3)),
                finished(),
            ]);
            let mut req = request();
            req.mode = RunMode::Supervised;
            let approver = Arc::new(Approvals::new(scope));
            let report = run(&deps, req, approver.clone());
            assert_eq!(report.outcome, Outcome::Finished);
            assert_eq!(approver.count.load(Ordering::SeqCst), expected);
            assert_eq!(host.count("set_many"), 2);
            cleanup(deps);
        }
    }

    #[test]
    fn three_denials_stop_the_rest_of_the_same_tool_batch() {
        let (deps, _, host) = fixture(vec![batch(vec![
            ("a", "set_camera_features", set_args(1)),
            ("b", "set_camera_features", set_args(2)),
            ("c", "set_camera_features", set_args(3)),
            (
                "d",
                "capture_photos",
                json!({"camera":"sim:0","count":1,"format":null,"file_name":null,"destination":null,"tags":null}),
            ),
        ])]);
        let mut req = request();
        req.mode = RunMode::Supervised;
        let report = run(&deps, req, Arc::new(DenyApprover::default()));
        assert_eq!(report.outcome, Outcome::NeedsUser);
        assert_eq!(host.count("set_many"), 0);
        assert_eq!(host.count("capture"), 0);
        cleanup(deps);
    }

    #[test]
    fn approval_rechecks_human_changes_before_writing() {
        let (deps, script, host) = fixture(vec![
            reply("set", "set_camera_features", set_args(2)),
            finished(),
        ]);
        let mut req = request();
        req.mode = RunMode::Supervised;
        let mut approvals = Approvals::new(ApprovalScope::Once);
        approvals.host = Some(host.clone());
        let report = run(&deps, req, Arc::new(approvals));
        assert_eq!(report.outcome, Outcome::Finished);
        assert_eq!(host.count("set_many"), 0);
        assert!(
            script.requests.lock().unwrap()[1]
                .1
                .to_string()
                .contains("action changed")
        );
        cleanup(deps);
    }

    struct Hook {
        script: Arc<Script>,
        once: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    }
    impl Transport for Hook {
        fn send(
            &self,
            request: &HttpRequest<'_>,
        ) -> std::result::Result<HttpResponse, ProviderError> {
            let response = self.script.send(request);
            if let Some(hook) = self.once.lock().unwrap().take() {
                hook();
            }
            response
        }
    }
    #[test]
    fn stop_discards_a_late_response_from_an_inflight_transport() {
        let (mut deps, script, host) =
            fixture(vec![reply("set", "set_camera_features", set_args(2))]);
        let control = RunControl::default();
        let stop = control.clone();
        let (entered, wait_entered) = mpsc::channel();
        let (release, wait_release) = mpsc::channel();
        deps.http = Arc::new(Http::with_transport(Arc::new(Hook {
            script,
            once: Mutex::new(Some(Box::new(move || {
                entered.send(()).unwrap();
                wait_release.recv().unwrap();
            }))),
        })));
        let path = deps.log_directory.clone();
        let task = thread::spawn(move || {
            run_blocking(
                request(),
                &deps,
                Arc::new(DenyApprover::default()),
                &control,
                &mut |_| {},
            )
        });
        wait_entered.recv_timeout(Duration::from_secs(5)).unwrap();
        stop.stop();
        release.send(()).unwrap();
        let report = task.join().unwrap().unwrap();
        assert_eq!(report.outcome, Outcome::Stopped);
        assert_eq!(host.count("set_many"), 0);
        let events = super::super::log::read_log_in(&path, &report.run_id).unwrap();
        assert_eq!(events.last().unwrap()["kind"], "end");
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn human_change_pauses_before_a_pending_capture_and_preserves_messages() {
        let photo = json!({"camera":"sim:0","count":1,"format":null,"file_name":null,"destination":null,"tags":null});
        let (mut deps, script, host) =
            fixture(vec![reply("capture", "capture_photos", photo), finished()]);
        let changed = host.clone();
        deps.http = Arc::new(Http::with_transport(Arc::new(Hook {
            script,
            once: Mutex::new(Some(Box::new(move || changed.gain(7)))),
        })));
        let mut req = request();
        req.on_human_change = OnHumanChange::Pause;
        let control = RunControl::default();
        let mut pauses = 0;
        let report = run_blocking(
            req,
            &deps,
            Arc::new(DenyApprover::default()),
            &control,
            &mut |event| {
                if matches!(
                    event,
                    RunEvent::State(RunSnapshot {
                        state: RunState::Paused,
                        ..
                    })
                ) {
                    assert_eq!(host.count("capture"), 0);
                    pauses += 1;
                    control
                        .message("Continue with the current camera settings.")
                        .unwrap();
                    control.resume();
                }
            },
        )
        .unwrap();
        assert_eq!(report.outcome, Outcome::Finished);
        assert_eq!(pauses, 1);
        assert_eq!(host.count("capture"), 1);
        cleanup(deps);
    }

    #[test]
    fn scope_and_destination_cannot_be_widened_by_tool_arguments() {
        let mut outside = set_args(2);
        outside["camera"] = json!("sim:1");
        let photo = json!({"camera":"sim:0","count":1,"format":null,"file_name":null,"destination":"line1","tags":null});
        let (deps, script, host) = fixture(vec![
            batch(vec![
                ("a", "set_camera_features", outside),
                ("b", "capture_photos", photo),
            ]),
            finished(),
        ]);
        let report = run(
            &deps,
            request(),
            Arc::new(Approvals::new(ApprovalScope::ForRun)),
        );
        assert_eq!(report.outcome, Outcome::Finished);
        assert_eq!(host.count("set_many"), 0);
        assert_eq!(host.count("capture"), 0);
        let request = script.requests.lock().unwrap()[1].1.to_string();
        assert!(request.contains("not part of this task"));
        cleanup(deps);
    }

    #[test]
    fn projected_and_actual_usage_stop_before_an_action() {
        for projected in [true, false] {
            let mut response = reply("set", "set_camera_features", set_args(2));
            response["usage"]["input_tokens"] = json!(1_000_000);
            let (deps, script, host) = fixture(vec![response]);
            let mut req = request();
            req.limits.max_input_tokens = if projected { 1 } else { 500_000 };
            let report = run(&deps, req, Arc::new(DenyApprover::default()));
            assert_eq!(report.outcome, Outcome::Limit);
            assert_eq!(host.count("set_many"), 0);
            assert_eq!(
                script.requests.lock().unwrap().len(),
                usize::from(!projected)
            );
            cleanup(deps);
        }
    }

    #[test]
    fn guards_stop_identical_helpers_and_consecutive_tool_errors() {
        for identical in [true, false] {
            let calls = (0..5)
                .map(|n| {
                    if identical {
                        reply(
                            &format!("call{n}"),
                            "remember",
                            json!({"notes":"same note"}),
                        )
                    } else {
                        reply(&format!("call{n}"), &format!("unknown{n}"), json!({}))
                    }
                })
                .collect();
            let (deps, script, _) = fixture(calls);
            let report = run(&deps, request(), Arc::new(DenyApprover::default()));
            assert_eq!(report.outcome, Outcome::Limit);
            assert_eq!(
                script.requests.lock().unwrap().len(),
                if identical { 4 } else { 5 }
            );
            cleanup(deps);
        }
    }

    #[test]
    fn settings_budget_is_reserved_before_the_worker_write() {
        let (deps, _, host) = fixture(vec![reply("set", "set_camera_features", set_args(2))]);
        let mut req = request();
        req.limits.max_setting_changes = 0;
        let report = run(&deps, req, Arc::new(DenyApprover::default()));
        assert_eq!(report.outcome, Outcome::Limit);
        assert_eq!(host.count("set_many"), 0);
        cleanup(deps);
    }

    #[test]
    fn restore_skips_human_changed_reconnected_and_banked_settings() {
        let host = Host::new();
        host.gain(2);
        let ctx = fixture_context(host.snapshot().unwrap());
        let pid = ctx
            .snapshot
            .cameras
            .iter()
            .find(|camera| camera.info.id == "sim:0")
            .unwrap()
            .worker_pid;
        let mut record = RunRecord {
            worker_pids: BTreeMap::from([("sim:0".into(), pid)]),
            settings: vec![SettingChange {
                camera: "sim:0".into(),
                feature: "Gain".into(),
                before: "0".into(),
                last: "2".into(),
                needs_review: false,
            }],
            ..RunRecord::default()
        };
        assert_eq!(restore(&record, &ctx).calls.len(), 1);
        host.gain(7);
        let changed = fixture_context(host.snapshot().unwrap());
        assert_eq!(restore(&record, &changed).skipped.len(), 1);
        record.worker_pids.insert("sim:0".into(), pid + 1);
        assert_eq!(restore(&record, &ctx).skipped.len(), 1);
        record.worker_pids.insert("sim:0".into(), pid);
        record.settings[0].needs_review = true;
        assert_eq!(restore(&record, &ctx).skipped.len(), 1);
    }

    #[test]
    fn no_call_reply_returns_needs_user_without_background_polling() {
        let (deps, script, host) = fixture(vec![end_reply(ProviderId::Anthropic)]);
        let report = run(&deps, request(), Arc::new(DenyApprover::default()));
        assert_eq!(report.outcome, Outcome::NeedsUser);
        assert_eq!(script.requests.lock().unwrap().len(), 1);
        assert_eq!(host.count("set_many"), 0);
        cleanup(deps);
    }

    #[test]
    fn a_partial_restart_failure_keeps_acknowledged_changes_restorable() {
        let args = json!({"camera":"sim:0","values":[{"feature":"Width","value":"638","expect":"640"}],"restart_stream":true});
        let (deps, _, host) = fixture(vec![reply("set", "set_camera_features", args), finished()]);
        host.snapshot
            .lock()
            .unwrap()
            .cameras
            .iter_mut()
            .find(|camera| camera.info.id == "sim:0")
            .unwrap()
            .streaming = true;
        host.fail_start.store(true, Ordering::SeqCst);
        let report = run(&deps, request(), Arc::new(DenyApprover::default()));
        assert_eq!(report.outcome, Outcome::Finished);
        assert_eq!(host.count("stop"), 1);
        assert_eq!(host.count("set_many"), 1);
        // Executor retries restoring the original live view once after the
        // failed sequence's Start, while retaining the successful setting ack.
        assert_eq!(host.count("start"), 2);
        assert_eq!(report.restore.settings.len(), 1);
        let row = &report.restore.settings[0];
        assert_eq!(
            (&*row.feature, &*row.before, &*row.last),
            ("Width", "640", "638")
        );
        assert_eq!(
            restore(&report.restore, &deps.gather(deps.host.as_ref()).unwrap())
                .calls
                .len(),
            1
        );
        cleanup(deps);
    }

    #[test]
    fn undo_reserves_the_exact_history_count_and_refuses_unknown_counts() {
        for count in [Some(3), None] {
            let (deps, _, host) = fixture(vec![
                reply(
                    "undo",
                    "undo_camera_change",
                    json!({"camera":"sim:0","token":"previous-change"}),
                ),
                finished(),
            ]);
            host.snapshot
                .lock()
                .unwrap()
                .cameras
                .iter_mut()
                .find(|camera| camera.info.id == "sim:0")
                .unwrap()
                .history
                .entries = vec![crate::session::CameraHistoryEntry {
                token: "previous-change".into(),
                label: "Three feature changes".into(),
                applied: true,
                created_at_ms: NOW,
                last_changed_at_us: 0,
                feature: None,
                kind: "configuration".into(),
                setting_changes: count,
            }];
            let mut req = request();
            req.limits.max_setting_changes = 2;
            let report = run(&deps, req, Arc::new(Approvals::new(ApprovalScope::Once)));
            assert_eq!(
                report.outcome,
                if count.is_some() {
                    Outcome::Limit
                } else {
                    Outcome::Finished
                }
            );
            assert_eq!(host.count("replay_camera_change"), 0);
            cleanup(deps);
        }
    }

    #[test]
    fn invalid_limits_are_rejected_before_credentials_or_http() {
        let (mut deps, script, _) = fixture(vec![]);
        let credentials = Arc::new(crate::assistant::agent::tests::Credentials(
            AtomicUsize::new(0),
        ));
        deps.credentials = credentials.clone();
        let mut req = request();
        req.limits.max_wall_ms = 0;
        assert!(
            run_blocking(
                req,
                &deps,
                Arc::new(DenyApprover::default()),
                &RunControl::default(),
                &mut |_| {}
            )
            .is_err()
        );
        assert_eq!(credentials.0.load(Ordering::SeqCst), 0);
        assert!(script.requests.lock().unwrap().is_empty());
        cleanup(deps);
    }

    #[test]
    fn post_start_context_failure_has_an_end_record() {
        struct FailSecond(AtomicUsize);
        impl ContextSource for FailSecond {
            fn gather(&self, host: &dyn ToolHost) -> Result<Context> {
                if self.0.fetch_add(1, Ordering::SeqCst) > 0 {
                    bail!("The fixture snapshot failed.");
                }
                Ok(fixture_context(host.snapshot()?))
            }
        }
        let (mut deps, _, _) = fixture(vec![]);
        deps.context = Arc::new(FailSecond(AtomicUsize::new(0)));
        assert!(
            run_blocking(
                request(),
                &deps,
                Arc::new(DenyApprover::default()),
                &RunControl::default(),
                &mut |_| {}
            )
            .is_err()
        );
        let runs = super::super::log::runs_in(&deps.log_directory).unwrap();
        assert_eq!(runs.len(), 1);
        assert!(runs[0].complete);
        assert_eq!(runs[0].outcome.as_deref(), Some("failed"));
        cleanup(deps);
    }

    #[test]
    fn pause_turn_continuations_obey_the_minimum_request_interval() {
        let pause = json!({"content":[{"type":"text","text":"Continuing the task."}],"stop_reason":"pause_turn","usage":{"input_tokens":10,"output_tokens":10}});
        let (deps, script, _) = fixture(vec![pause, finished()]);
        let report = run(&deps, request(), Arc::new(DenyApprover::default()));
        assert_eq!(report.outcome, Outcome::Finished);
        assert_eq!(script.requests.lock().unwrap().len(), 2);
        assert!(deps.clock.now_ms() >= NOW + 1000);
        cleanup(deps);
    }

    #[test]
    fn an_auto_once_command_does_not_pause_on_its_own_feature_changes() {
        let (deps, _, host) = fixture(vec![
            reply(
                "auto",
                "run_camera_command",
                json!({"camera":"sim:0","command":"GainAutoOnce"}),
            ),
            finished(),
        ]);
        host.snapshot
            .lock()
            .unwrap()
            .cameras
            .iter_mut()
            .find(|camera| camera.info.id == "sim:0")
            .unwrap()
            .features
            .push(crate::tools::tests::feature(
                "GainAutoOnce",
                "Command",
                Value::Null,
            ));
        let mut req = request();
        req.on_human_change = OnHumanChange::Pause;
        let report = run_blocking(
            req,
            &deps,
            Arc::new(DenyApprover::default()),
            &RunControl::default(),
            &mut |event| {
                assert!(
                    !matches!(
                        event,
                        RunEvent::State(RunSnapshot {
                            state: RunState::Paused,
                            ..
                        })
                    ),
                    "The run must recognize its own auto command."
                )
            },
        )
        .unwrap();
        assert_eq!(report.outcome, Outcome::Finished);
        assert_eq!(host.count("execute"), 1);
        assert!(
            report
                .restore
                .settings
                .iter()
                .any(|row| row.feature == "Gain" && row.last == "2")
        );
        cleanup(deps);
    }

    #[test]
    #[cfg(feature = "s3")]
    fn a_missing_human_secret_reaches_approval_but_never_executes_without_it() {
        struct SecretApproval(Mutex<Vec<ApprovalRequest>>);
        impl Approver for SecretApproval {
            fn request(
                &self,
                req: ApprovalRequest,
                _: &AtomicBool,
                _: Duration,
            ) -> ApprovalDecision {
                self.0.lock().unwrap().push(req);
                ApprovalDecision::Approved {
                    scope: ApprovalScope::Once,
                    by: SessionActor::new("desktop:secret-test", "Desktop", ActorKind::Human)
                        .unwrap(),
                }
            }
        }
        let args = json!({"name":"new-archive","bucket":"camera-archive","region":"us-east-1","endpoint":null,"prefix":null,"path_style":null,"credentials":"keychain","profile":null,"access_key_id":"AKIAIOSFODNN7EXAMPLE","keep_local":null});
        let (deps, script, host) = fixture(vec![
            reply("destination", "create_s3_destination", args),
            finished(),
        ]);
        let approver = Arc::new(SecretApproval(Mutex::new(Vec::new())));
        let report = run(&deps, request(), approver.clone());
        assert_eq!(report.outcome, Outcome::Finished);
        let approvals = approver.0.lock().unwrap();
        assert_eq!(approvals.len(), 1);
        assert!(approvals[0].needs_secret);
        assert!(!approvals[0].allow_run_scope);
        assert_eq!(approvals[0].step, StepId(1));
        assert!(host.sent.lock().unwrap().is_empty());
        let bodies = script.requests.lock().unwrap();
        let text = serde_json::to_string(&bodies[1].1).unwrap();
        assert!(text.contains("changed while it was being reviewed"));
        drop(bodies);
        drop(approvals);
        cleanup(deps);
    }

    #[test]
    fn observation_wait_publishes_a_source_checked_waiting_state() {
        let args = json!({"camera":"sim:0","until":"recording_stopped","timeout":"1s","frames":null,"job_id":null});
        let (deps, _, _) = fixture(vec![reply("wait", "wait_for", args), finished()]);
        let mut waiting = Vec::new();
        let report = run_blocking(
            request(),
            &deps,
            Arc::new(DenyApprover::default()),
            &RunControl::default(),
            &mut |event| {
                if let RunEvent::State(state) = event
                    && state.state == RunState::Waiting
                {
                    waiting.push(state.summary);
                }
            },
        )
        .unwrap();
        assert_eq!(report.outcome, Outcome::Finished);
        assert_eq!(waiting.len(), 1);
        assert!(waiting[0].contains("recording"));
        cleanup(deps);
    }

    #[test]
    fn quit_during_credential_resolution_synchronously_finishes_the_registered_journal() {
        struct Credentials {
            entered: std::sync::mpsc::Sender<()>,
            release: Mutex<std::sync::mpsc::Receiver<()>>,
        }
        impl super::super::agent::CredentialResolver for Credentials {
            fn resolve(
                &self,
                id: ProviderId,
                settings: &super::super::ProviderSettings,
            ) -> Result<super::super::credentials::ResolvedCredentials> {
                self.entered.send(()).unwrap();
                self.release
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap();
                super::super::agent::CredentialResolver::resolve(
                    &super::super::agent::tests::Credentials(AtomicUsize::new(0)),
                    id,
                    settings,
                )
            }
        }
        let (mut deps, script, _) = fixture(vec![finished()]);
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        deps.credentials = Arc::new(Credentials {
            entered: entered_tx,
            release: Mutex::new(release_rx),
        });
        let directory = deps.log_directory.clone();
        let deps = Arc::new(deps);
        let control = RunControl::default();
        let worker = control.clone();
        let worker_deps = deps.clone();
        let thread = thread::spawn(move || {
            run_blocking(
                request(),
                &worker_deps,
                Arc::new(DenyApprover::default()),
                &worker,
                &mut |_| {},
            )
        });
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let run_id = control.status().run_id;
        assert!(!run_id.is_empty());
        control.finish_app_quit();
        let records = super::super::log::read_log_in(&directory, &run_id).unwrap();
        assert_eq!(records.last().unwrap()["outcome"], "app_quit");
        release_tx.send(()).unwrap();
        assert_eq!(thread.join().unwrap().unwrap().outcome, Outcome::AppQuit);
        let records = super::super::log::read_log_in(&directory, &run_id).unwrap();
        assert_eq!(records.iter().filter(|r| r["kind"] == "end").count(), 1);
        assert_eq!(records.last().unwrap()["outcome"], "app_quit");
        assert!(script.requests.lock().unwrap().is_empty());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn quitting_claims_the_terminal_outcome_before_cancellation_reaches_the_worker() {
        let (deps, _, _) = fixture(vec![finished()]);
        let mut log = ActionLog::create(&deps.log_directory, NOW).unwrap();
        log.record("start", json!({"goal":"Test application quit."}), NOW)
            .unwrap();
        let run_id = log.id().to_owned();
        let log = Arc::new(Mutex::new(log));
        let control = RunControl::default();
        *control.shared.log.lock().unwrap() = Some(log.clone());

        // Hold the journal where the old stop-then-finish ordering could
        // pause after publishing cancellation but before claiming the end.
        let journal = log.lock().unwrap();
        let quitter = control.clone();
        let thread = thread::spawn(move || quitter.finish_app_quit());
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !control.cancel().load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
            thread::yield_now();
        }
        let cancelled = control.cancel().load(Ordering::SeqCst);
        let quit_already_claimed_end = control.shared.ended.swap(true, Ordering::SeqCst);
        drop(journal);
        thread.join().unwrap();

        assert!(cancelled);
        assert!(quit_already_claimed_end);
        let records = super::super::log::read_log_in(&deps.log_directory, &run_id).unwrap();
        assert_eq!(records.iter().filter(|r| r["kind"] == "end").count(), 1);
        assert_eq!(records.last().unwrap()["outcome"], "app_quit");
        cleanup(deps);
    }

    #[test]
    fn startup_stop_and_wall_limit_finish_before_a_noncooperative_resolver() {
        struct Blocked {
            entered: mpsc::Sender<()>,
            release: Mutex<mpsc::Receiver<()>>,
        }
        impl super::super::agent::CredentialResolver for Blocked {
            fn resolve(
                &self,
                id: ProviderId,
                settings: &super::super::ProviderSettings,
            ) -> Result<super::super::credentials::ResolvedCredentials> {
                self.entered.send(()).unwrap();
                self.release
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap();
                super::super::agent::CredentialResolver::resolve(
                    &super::super::agent::tests::Credentials(AtomicUsize::new(0)),
                    id,
                    settings,
                )
            }
        }
        for limit in [false, true] {
            let (mut deps, script, host) = fixture(vec![finished()]);
            let directory = deps.log_directory.clone();
            let clock = Arc::new(ClockStub(AtomicU64::new(NOW)));
            deps.clock = clock.clone();
            let (entered_tx, entered_rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel();
            deps.credentials = Arc::new(Blocked {
                entered: entered_tx,
                release: Mutex::new(release_rx),
            });
            let control = RunControl::default();
            let worker = control.clone();
            let (done_tx, done_rx) = mpsc::channel();
            let thread = thread::spawn(move || {
                let mut req = request();
                req.limits.max_wall_ms = 60_000;
                let result = run_blocking(
                    req,
                    &deps,
                    Arc::new(DenyApprover::default()),
                    &worker,
                    &mut |_| {},
                );
                let _ = done_tx.send(result);
            });
            entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            if limit {
                clock.0.store(NOW + 60_001, Ordering::SeqCst);
            } else {
                control.stop();
            }
            let report = done_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("startup must not wait for the resolver")
                .unwrap();
            assert_eq!(
                report.outcome,
                if limit {
                    Outcome::Limit
                } else {
                    Outcome::Stopped
                }
            );
            assert!(control.is_finished());
            let records = super::super::log::read_log_in(&directory, &report.run_id).unwrap();
            assert_eq!(
                records.last().unwrap()["outcome"],
                if limit { "limit" } else { "stopped" }
            );
            assert!(script.requests.lock().unwrap().is_empty());
            assert!(host.sent.lock().unwrap().is_empty());
            release_tx.send(()).unwrap();
            thread.join().unwrap();
            std::fs::remove_dir_all(directory).unwrap();
        }
    }

    #[test]
    fn vertex_rounds_reuse_cached_tokens_and_refresh_only_the_pinned_auth() {
        use super::super::credentials::{Auth, ResolvedCredentials, Source};
        use super::super::http::Headers;

        struct GoogleCredentials {
            clock: Arc<ClockStub>,
            identity: String,
            exchanges: AtomicUsize,
        }
        impl Source for GoogleCredentials {
            fn environment(&self, name: &str) -> Result<Option<String>> {
                Ok((name == "GOOGLE_APPLICATION_CREDENTIALS").then(|| self.identity.clone()))
            }
            fn keychain(&self, _: &str) -> Result<Option<String>> {
                panic!("the offline ADC test must never read a keychain")
            }
            fn file(&self, _: &std::path::Path) -> Result<Option<zeroize::Zeroizing<Vec<u8>>>> {
                Ok(Some(zeroize::Zeroizing::new(serde_json::to_vec(&json!({
                    "type":"authorized_user", "client_id":"offline-client",
                    "client_secret":"offline-client-secret", "refresh_token":self.identity,
                    "quota_project_id":"offline-refresh-project"
                }))?)))
            }
            fn gcloud(&self) -> Result<Option<PathBuf>> {
                Ok(None)
            }
            fn run_gcloud(&self, _: &std::path::Path) -> Result<zeroize::Zeroizing<String>> {
                panic!("the offline ADC test must never launch gcloud")
            }
            fn metadata_allowed(&self) -> bool {
                false
            }
            fn send_json(&self, request: HttpRequest<'_>) -> Result<(Value, Headers)> {
                assert_eq!(request.url, "https://oauth2.googleapis.com/token");
                let exchange = self.exchanges.fetch_add(1, Ordering::SeqCst) + 1;
                Ok((
                    json!({"access_token":format!("offline-google-token-{exchange}"),"token_type":"Bearer","expires_in":3600}),
                    Headers::new(),
                ))
            }
            fn aws_chain(&self, _: Option<&str>) -> Result<(crate::aws::Keys, String)> {
                panic!("the offline ADC test must never read AWS credentials")
            }
            fn aws_region(&self, _: &str) -> Option<String> {
                None
            }
            fn now(&self) -> u64 {
                self.clock.now_ms() / 1000
            }
            fn cache_tokens(&self) -> bool {
                true
            }
        }
        impl super::super::agent::CredentialResolver for GoogleCredentials {
            fn resolve(
                &self,
                id: ProviderId,
                settings: &super::super::ProviderSettings,
            ) -> Result<ResolvedCredentials> {
                assert_eq!(id, ProviderId::Vertex);
                let token = super::super::google::resolve(self)?.unwrap();
                Ok(ResolvedCredentials {
                    auth: Auth::Header {
                        name: "authorization",
                        value: zeroize::Zeroizing::new(format!("Bearer {}", token.value.as_str())),
                        quota_project: token.quota_project,
                    },
                    settings: settings.effective(id, |_| None).unwrap(),
                })
            }
        }
        struct TokenScript {
            script: Arc<Script>,
            clock: Arc<ClockStub>,
            auth: Mutex<Vec<String>>,
        }
        impl Transport for TokenScript {
            fn send(&self, request: &HttpRequest<'_>) -> Result<HttpResponse, ProviderError> {
                let Auth::Header { name, value, .. } = request.auth else {
                    panic!("expected the pinned Google authorization kind")
                };
                assert_eq!(*name, "authorization");
                let round = {
                    let mut auth = self.auth.lock().unwrap();
                    auth.push(value.to_string());
                    auth.len()
                };
                let response = self.script.send(request)?;
                let elapsed = match round {
                    1 => 3_299_000,
                    2 => 1000,
                    _ => 0,
                };
                self.clock.0.fetch_add(elapsed, Ordering::SeqCst);
                Ok(response)
            }
        }
        let generate = |id: &str, name: &str, args: Value| json!({"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"id":id,"name":name,"args":args}}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":100,"candidatesTokenCount":10}});
        let replies = vec![
            generate("first", "remember", json!({"notes":"First measurement."})),
            generate("second", "remember", json!({"notes":"Second measurement."})),
            generate(
                "finish",
                "finish_task",
                json!({"outcome":"succeeded","summary":"Verified the camera."}),
            ),
        ];
        let (mut deps, script, host) = fixture(replies);
        let clock = Arc::new(ClockStub(AtomicU64::new(NOW)));
        let credentials = Arc::new(GoogleCredentials {
            clock: clock.clone(),
            identity: deps
                .log_directory
                .join("offline-adc")
                .to_string_lossy()
                .into_owned(),
            exchanges: AtomicUsize::new(0),
        });
        let transport = Arc::new(TokenScript {
            script: script.clone(),
            clock: clock.clone(),
            auth: Mutex::new(Vec::new()),
        });
        deps.clock = clock;
        deps.credentials = credentials.clone();
        deps.http = Arc::new(Http::with_transport(transport.clone()));
        deps.settings.provider = Some(ProviderId::Vertex);
        deps.settings.providers.insert(
            ProviderId::Vertex,
            super::super::ProviderSettings {
                project: Some("offline-refresh-project".into()),
                location: Some("global".into()),
                ..super::super::ProviderSettings::default()
            },
        );
        let mut req = request();
        req.limits.max_wall_ms = 14_400_000;
        let report = run(&deps, req, Arc::new(DenyApprover::default()));
        assert_eq!(report.outcome, Outcome::Finished);
        assert_eq!(report.totals.rounds, 3);
        assert_eq!(credentials.exchanges.load(Ordering::SeqCst), 2);
        assert_eq!(
            transport.auth.lock().unwrap().as_slice(),
            [
                "Bearer offline-google-token-1",
                "Bearer offline-google-token-1",
                "Bearer offline-google-token-2",
            ]
        );
        let requests = script.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests.windows(2).all(|pair| pair[0].0 == pair[1].0));
        assert!(
            requests[2].1["contents"].as_array().unwrap().len()
                > requests[0].1["contents"].as_array().unwrap().len()
        );
        assert!(host.sent.lock().unwrap().is_empty());
        drop(requests);
        cleanup(deps);
    }

    #[test]
    fn a_vertex_refresh_cannot_outlive_the_run_or_send_a_late_model_request() {
        use super::super::credentials::{Auth, ResolvedCredentials};
        struct Credentials {
            refreshes: AtomicUsize,
            entered: mpsc::Sender<()>,
            release: Mutex<mpsc::Receiver<()>>,
            finished: mpsc::Sender<()>,
        }
        impl Credentials {
            fn auth() -> Auth {
                Auth::Header {
                    name: "authorization",
                    value: zeroize::Zeroizing::new("Bearer offline-refresh-token".into()),
                    quota_project: None,
                }
            }
        }
        impl super::super::agent::CredentialResolver for Credentials {
            fn resolve(
                &self,
                id: ProviderId,
                settings: &super::super::ProviderSettings,
            ) -> Result<ResolvedCredentials> {
                Ok(ResolvedCredentials {
                    auth: Self::auth(),
                    settings: settings.effective(id, |_| None).unwrap(),
                })
            }
            fn refresh_vertex(
                &self,
                _: &super::super::ProviderSettings,
                _: Duration,
                _: &AtomicBool,
            ) -> Result<Auth> {
                if self.refreshes.fetch_add(1, Ordering::SeqCst) == 1 {
                    self.entered.send(()).unwrap();
                    self.release
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(5))
                        .unwrap();
                    self.finished.send(()).unwrap();
                }
                Ok(Self::auth())
            }
        }
        let first = json!({"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"id":"remember","name":"remember","args":{"notes":"A measurement."}}}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":100,"candidatesTokenCount":10}});
        let (mut deps, script, host) = fixture(vec![first]);
        let clock = Arc::new(ClockStub(AtomicU64::new(NOW)));
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        deps.clock = clock.clone();
        deps.credentials = Arc::new(Credentials {
            refreshes: AtomicUsize::new(0),
            entered: entered_tx,
            release: Mutex::new(release_rx),
            finished: finished_tx,
        });
        deps.settings.provider = Some(ProviderId::Vertex);
        deps.settings.providers.insert(
            ProviderId::Vertex,
            super::super::ProviderSettings {
                project: Some("offline-refresh-project".into()),
                location: Some("global".into()),
                ..super::super::ProviderSettings::default()
            },
        );
        let directory = deps.log_directory.clone();
        let deps = Arc::new(deps);
        let worker_deps = deps.clone();
        let (done_tx, done_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let mut req = request();
            req.limits.max_wall_ms = 60_000;
            done_tx
                .send(run(&worker_deps, req, Arc::new(DenyApprover::default())))
                .unwrap();
        });
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        clock.0.store(NOW + 60_001, Ordering::SeqCst);
        let report = done_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(report.outcome, Outcome::Limit);
        assert_eq!(report.totals.rounds, 1);
        assert_eq!(script.requests.lock().unwrap().len(), 1);
        assert!(host.sent.lock().unwrap().is_empty());
        release_tx.send(()).unwrap();
        finished_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        worker.join().unwrap();
        assert_eq!(script.requests.lock().unwrap().len(), 1);
        drop(deps);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn pausing_during_a_vertex_refresh_waits_before_starting_a_model_round() {
        use super::super::credentials::{Auth, ResolvedCredentials};
        struct Credentials {
            refreshes: AtomicUsize,
            entered: mpsc::Sender<()>,
            release: Mutex<mpsc::Receiver<()>>,
        }
        impl Credentials {
            fn auth() -> Auth {
                Auth::Header {
                    name: "authorization",
                    value: zeroize::Zeroizing::new("Bearer offline-pause-token".into()),
                    quota_project: None,
                }
            }
        }
        impl super::super::agent::CredentialResolver for Credentials {
            fn resolve(
                &self,
                id: ProviderId,
                settings: &super::super::ProviderSettings,
            ) -> Result<ResolvedCredentials> {
                Ok(ResolvedCredentials {
                    auth: Self::auth(),
                    settings: settings.effective(id, |_| None).unwrap(),
                })
            }
            fn refresh_vertex(
                &self,
                _: &super::super::ProviderSettings,
                _: Duration,
                _: &AtomicBool,
            ) -> Result<Auth> {
                if self.refreshes.fetch_add(1, Ordering::SeqCst) == 0 {
                    self.entered.send(()).unwrap();
                    self.release
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(5))
                        .unwrap();
                }
                Ok(Self::auth())
            }
        }
        let finish = json!({"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"id":"finish","name":"finish_task","args":{"outcome":"succeeded","summary":"Verified the camera."}}}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":100,"candidatesTokenCount":10}});
        let (mut deps, script, host) = fixture(vec![finish]);
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let credentials = Arc::new(Credentials {
            refreshes: AtomicUsize::new(0),
            entered: entered_tx,
            release: Mutex::new(release_rx),
        });
        deps.credentials = credentials.clone();
        deps.settings.provider = Some(ProviderId::Vertex);
        deps.settings.providers.insert(
            ProviderId::Vertex,
            super::super::ProviderSettings {
                project: Some("offline-refresh-project".into()),
                location: Some("global".into()),
                ..super::super::ProviderSettings::default()
            },
        );
        let deps = Arc::new(deps);
        let control = RunControl::default();
        let worker_control = control.clone();
        let worker_deps = deps.clone();
        let (paused_tx, paused_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let mut req = request();
            req.limits.max_wall_ms = 86_400_000;
            run_blocking(
                req,
                &worker_deps,
                Arc::new(DenyApprover::default()),
                &worker_control,
                &mut |event| {
                    if matches!(event, RunEvent::State(state) if state.state == RunState::Paused) {
                        let _ = paused_tx.send(());
                    }
                },
            )
            .unwrap()
        });
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        control.pause();
        release_tx.send(()).unwrap();
        paused_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(script.requests.lock().unwrap().is_empty());
        assert_eq!(control.status().totals.rounds, 0);
        control.resume();
        let report = worker.join().unwrap();
        assert_eq!(report.outcome, Outcome::Finished);
        assert_eq!(report.totals.rounds, 1);
        assert_eq!(script.requests.lock().unwrap().len(), 1);
        assert_eq!(credentials.refreshes.load(Ordering::SeqCst), 2);
        assert!(host.sent.lock().unwrap().is_empty());
        let directory = deps.log_directory.clone();
        drop(deps);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_model_reply_after_the_wall_deadline_cannot_run_a_camera_action() {
        let (mut deps, script, host) =
            fixture(vec![reply("set", "set_camera_features", set_args(2))]);
        let clock = Arc::new(ClockStub(AtomicU64::new(NOW)));
        deps.clock = clock.clone();
        deps.http = Arc::new(Http::with_transport(Arc::new(Hook {
            script,
            once: Mutex::new(Some(Box::new(move || {
                clock.0.store(NOW + 60_001, Ordering::SeqCst);
            }))),
        })));
        let mut req = request();
        req.limits.max_wall_ms = 60_000;
        let report = run(&deps, req, Arc::new(DenyApprover::default()));
        assert_eq!(report.outcome, Outcome::Limit);
        assert_eq!(host.count("set_many"), 0);
        cleanup(deps);
    }

    #[test]
    fn a_context_failure_cancels_the_pending_approval_worker() {
        struct ContextsFail {
            root: PathBuf,
            fail: Arc<AtomicBool>,
        }
        impl ContextSource for ContextsFail {
            fn gather(&self, host: &dyn ToolHost) -> Result<Context> {
                ensure!(
                    !self.fail.load(Ordering::SeqCst),
                    "Injected context failure."
                );
                let mut ctx = fixture_context(host.snapshot()?);
                ctx.capture_root = self.root.clone();
                Ok(ctx)
            }
        }
        struct WaitingApproval {
            fail: Arc<AtomicBool>,
            done: mpsc::Sender<bool>,
        }
        impl Approver for WaitingApproval {
            fn request(
                &self,
                _: ApprovalRequest,
                cancel: &AtomicBool,
                _: Duration,
            ) -> ApprovalDecision {
                self.fail.store(true, Ordering::SeqCst);
                let deadline = std::time::Instant::now() + Duration::from_secs(3);
                while !cancel.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
                    thread::sleep(Duration::from_millis(1));
                }
                let cancelled = cancel.load(Ordering::SeqCst);
                let _ = self.done.send(cancelled);
                ApprovalDecision::Cancelled
            }
        }
        let (mut deps, _, host) = fixture(vec![reply("set", "set_camera_features", set_args(2))]);
        let fail = Arc::new(AtomicBool::new(false));
        deps.context = Arc::new(ContextsFail {
            root: deps.log_directory.join("captures"),
            fail: fail.clone(),
        });
        let (done_tx, done_rx) = mpsc::channel();
        let mut req = request();
        req.mode = RunMode::Supervised;
        let result = run_blocking(
            req,
            &deps,
            Arc::new(WaitingApproval {
                fail,
                done: done_tx,
            }),
            &RunControl::default(),
            &mut |_| {},
        );
        assert!(result.is_err());
        assert!(done_rx.recv_timeout(Duration::from_secs(1)).unwrap());
        assert_eq!(host.count("set_many"), 0);
        cleanup(deps);
    }

    #[test]
    fn accepted_recordings_remain_reported_when_verification_or_context_refresh_fails() {
        #[derive(Clone)]
        struct ForwardHost {
            base: Host,
            failed: Arc<AtomicBool>,
            started: Arc<AtomicBool>,
            persistent: bool,
        }
        impl ToolHost for ForwardHost {
            fn snapshot(&self) -> Result<SessionSnapshot> {
                if self.failed.swap(false, Ordering::SeqCst)
                    || self.persistent && self.started.load(Ordering::SeqCst)
                {
                    bail!("Injected forwarding status failure.");
                }
                self.base.snapshot()
            }
            fn has_session(&self) -> bool {
                true
            }
            fn actor_id(&self) -> Option<String> {
                self.base.actor_id()
            }
            fn with_actor(&self, actor: SessionActor) -> Result<Arc<dyn ToolHost>> {
                let mut host = self.clone();
                host.base.actor = Some(actor);
                Ok(Arc::new(host))
            }
            fn execute(&self, camera: Option<&str>, command: SessionCommand) -> Result<Value> {
                if let SessionCommand::Forward {
                    output,
                    duration_ms,
                    ..
                } = &command
                {
                    self.base.sent.lock().unwrap().push((
                        self.base.actor.clone(),
                        camera.map(str::to_owned),
                        command.clone(),
                    ));
                    let mut snapshot = self.base.snapshot.lock().unwrap();
                    let state = snapshot
                        .cameras
                        .iter_mut()
                        .find(|state| Some(state.info.id.as_str()) == camera)
                        .unwrap();
                    state.forwarding = Some(output.clone());
                    state.forwarding_id = Some("22222222222222222222222222222222".into());
                    state.forwarding_since_ms = Some(NOW);
                    state.forwarding_until_ms = duration_ms.map(|d| NOW + d);
                    self.started.store(true, Ordering::SeqCst);
                    self.failed.store(true, Ordering::SeqCst);
                    return Ok(
                        json!({"forwarding":output,"forwarding_id":"22222222222222222222222222222222","since_ms":NOW,"until_ms":state.forwarding_until_ms}),
                    );
                }
                self.base.execute(camera, command)
            }
        }
        for persistent in [false, true] {
            let args = json!({"camera":"sim:0","duration":"30s","file_name":null,"destination":null,"codec":null,"fps":null,"bitrate":null,"tags":null});
            let (mut deps, _, host) =
                fixture(vec![reply("record", "start_recording", args), finished()]);
            deps.host = Arc::new(ForwardHost {
                base: host,
                failed: Arc::new(AtomicBool::new(false)),
                started: Arc::new(AtomicBool::new(false)),
                persistent,
            });
            let mut req = request();
            req.scope.allow_recordings = true;
            let mut report = None;
            let result = run_blocking(
                req,
                &deps,
                Arc::new(DenyApprover::default()),
                &RunControl::default(),
                &mut |event| {
                    if let RunEvent::Finished(outcome) = event {
                        report = Some(outcome);
                    }
                },
            );
            assert_eq!(result.is_err(), persistent);
            let report = report.expect("accepted effects must remain reviewable");
            assert!(
                report
                    .leftovers
                    .iter()
                    .any(|left| left.kind == "recording" && left.camera == "sim:0")
            );
            assert_eq!(
                report.restore.forwarders.get("sim:0").map(String::as_str),
                Some("22222222222222222222222222222222")
            );
            cleanup(deps);
        }
    }

    #[test]
    fn quit_before_initialization_never_creates_a_late_action_log() {
        let (deps, script, _) = fixture(vec![finished()]);
        let control = RunControl::default();
        control.finish_app_quit();
        let result = run_blocking(
            request(),
            &deps,
            Arc::new(DenyApprover::default()),
            &control,
            &mut |_| {},
        );
        assert!(result.is_err());
        assert!(control.is_finished());
        assert!(script.requests.lock().unwrap().is_empty());
        assert!(
            super::super::log::runs_in(&deps.log_directory)
                .unwrap()
                .is_empty()
        );
        cleanup(deps);
    }

    #[test]
    fn reconnect_revokes_old_worker_ownership_grants_and_restore_rows() {
        let mut facts = RunFacts {
            jobs: BTreeSet::from([("sim:0".into(), 1), ("sim:1".into(), 1)]),
            tokens: vec![
                ("sim:0".into(), "old-token".into()),
                ("sim:1".into(), "other-token".into()),
            ],
            forwarders: BTreeSet::from(["sim:0".into(), "sim:1".into()]),
            worker_pids: BTreeMap::from([("sim:0".into(), 10), ("sim:1".into(), 11)]),
            ..RunFacts::default()
        };
        let mut record = RunRecord {
            worker_pids: facts.worker_pids.clone(),
            settings: ["sim:0", "sim:1"]
                .into_iter()
                .map(|camera| SettingChange {
                    camera: camera.into(),
                    feature: "Gain".into(),
                    before: "0".into(),
                    last: "2".into(),
                    needs_review: false,
                })
                .collect(),
            ..RunRecord::default()
        };
        let mut tokens = facts.tokens.clone();
        let mut policy = ApprovalPolicy::new(RunMode::Supervised);
        let mut checked = Checked::new(
            StepId(1),
            &ToolCall {
                tool: "set_camera_features".into(),
                args: set_args(2),
            },
        );
        checked.risk = Risk::Reversible;
        policy.record(
            &checked,
            &ApprovalDecision::Approved {
                scope: ApprovalScope::ForRun,
                by: SessionActor::new("desktop:test", "Desktop", ActorKind::Human).unwrap(),
            },
        );
        assert!(policy.granted(&checked).is_some());
        let mut forwarders = BTreeMap::from([
            ("sim:0".into(), "forward-one".into()),
            ("sim:1".into(), "forward-two".into()),
        ]);
        let mut auto = BTreeSet::from(["sim:0".into(), "sim:1".into()]);
        forget_worker(
            "sim:0",
            &mut facts,
            &mut record,
            &mut tokens,
            &mut policy,
            &mut forwarders,
            &mut auto,
        );
        assert_eq!(facts.jobs, BTreeSet::from([("sim:1".into(), 1)]));
        assert_eq!(tokens, vec![("sim:1".to_owned(), "other-token".to_owned())]);
        assert_eq!(facts.tokens, tokens);
        assert_eq!(facts.forwarders, BTreeSet::from(["sim:1".into()]));
        assert!(!facts.worker_pids.contains_key("sim:0"));
        assert!(!record.worker_pids.contains_key("sim:0"));
        assert!(!forwarders.contains_key("sim:0"));
        assert!(!auto.contains("sim:0"));
        assert!(record.settings[0].needs_review);
        assert!(!record.settings[1].needs_review);
        assert!(policy.granted(&checked).is_none());
    }
}
