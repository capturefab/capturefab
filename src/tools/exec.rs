//! Checked plans, conservative risk classification and pinned execution.
use super::{
    CameraRef, Issue, Origin, Risk, StepId, ToolCall, ToolKind,
    context::{Context, DestinationInfo, ToolHost},
    redact,
};
use crate::{destination::Destination, session::SessionCommand};
use anyhow::{Context as _, Result, anyhow, bail, ensure};
use serde::{Serialize, Serializer};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fmt,
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use zeroize::Zeroizing;

/// An operation whose camera identity was selected while checking the plan.
#[derive(Clone, Debug)]
pub enum Op {
    /// A checked coordinator operation. An earlier approved open may supply
    /// the dataset slot only when this flag is explicit.
    Session {
        command: SessionCommand,
        dataset_from_plan: bool,
    },
    Connect {
        selector: String,
        /// False for assistant plans, which never use a sign-in saved on
        /// this host; a camera a person connected stays usable.
        saved_login: bool,
    },
    Command {
        camera: CameraRef,
        command: SessionCommand,
    },
    /// Stop the checked recording, or one a preceding step in this executor
    /// started. An absent identity never means stop whatever is current.
    StopForward {
        camera: CameraRef,
        forwarding_id: Option<String>,
    },
    /// Stop, write and resume acquisition. A resume is attempted even when
    /// cancellation or a failed write interrupts the sequence.
    Restartable {
        camera: CameraRef,
        commands: Vec<SessionCommand>,
    },
    CreateDestination {
        destination: Destination,
    },
    /// A credential-free Forward; the password is merged in a temporary
    /// command immediately before sending it to the host.
    Stream {
        camera: CameraRef,
        command: SessionCommand,
    },
    Undo {
        camera: CameraRef,
        token: Option<String>,
    },
    Observe(ToolCall),
}

impl Serialize for Op {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let value = match self {
            Self::Session {
                command,
                dataset_from_plan,
            } => json!({"command":command,"dataset_from_plan":dataset_from_plan}),
            Self::Connect { selector, .. } => json!({"op":"connect","selector":selector}),
            Self::Command { camera, command } => {
                json!({"camera":camera.selector(),"command":command})
            }
            Self::StopForward {
                camera,
                forwarding_id,
            } => {
                json!({"op":"stop_forward_if","camera":camera.selector(),"forwarding_id":forwarding_id})
            }
            Self::Restartable { camera, commands } => {
                json!({"op":"restartable","camera":camera.selector(),"commands":commands})
            }
            Self::CreateDestination { destination } => {
                json!({"op":"create_destination","destination":destination})
            }
            Self::Stream { camera, command } => {
                json!({"camera":camera.selector(),"command":command})
            }
            Self::Undo { camera, token } => {
                json!({"op":"undo","camera":camera.selector(),"token":token})
            }
            Self::Observe(call) => json!({"op":"observe","call":call}),
        };
        redact::for_output(value).serialize(serializer)
    }
}

/// Everything a person approves, plus the operations to perform.
#[derive(Clone)]
pub struct Checked {
    /// Private resource identities used to bind scope and approvals.
    pub data_resources: BTreeSet<String>,
    pub id: StepId,
    pub call: ToolCall,
    pub title: String,
    pub summary: String,
    pub facts: Vec<(String, String)>,
    pub issues: Vec<Issue>,
    pub risk: Risk,
    pub reasons: Vec<String>,
    pub leaves_computer: bool,
    pub ops: Vec<Op>,
    /// Approval binding uses the actual root even when display redaction
    /// replaces its home prefix.
    pub output_root: Option<PathBuf>,
    pub destination: Option<DestinationInfo>,
    pub actor_id: Option<String>,
}

impl Checked {
    pub fn new(id: StepId, call: &ToolCall) -> Self {
        let spec = super::spec(&call.tool);
        let title = spec.map_or_else(|| call.tool.clone(), |spec| spec.title.into());
        Self {
            data_resources: BTreeSet::new(),
            id,
            call: call.clone(),
            summary: title.clone(),
            title,
            facts: Vec::new(),
            issues: Vec::new(),
            risk: spec.map_or(Risk::Observe, |spec| spec.risk),
            reasons: Vec::new(),
            leaves_computer: false,
            ops: Vec::new(),
            output_root: None,
            destination: None,
            actor_id: None,
        }
    }

    pub fn issue(&mut self, mut issue: Issue) {
        issue.message = redact::for_output_error(&issue.message, &self.call.args);
        self.issues.push(issue);
    }

    pub fn error(&mut self, field: &str, message: impl Into<String>) {
        self.issue(Issue::usage(Some(field), message));
    }

    pub fn warning(&mut self, field: &str, message: impl Into<String>) {
        self.issue(Issue::warning(Some(field), message));
    }

    pub fn ready(&self) -> bool {
        !self.issues.iter().any(Issue::is_error)
    }

    pub fn value(&self) -> Value {
        redact::for_output(json!({
            "id":self.id,"call":self.call,"title":self.title,"summary":self.summary,
            "facts":self.facts,"issues":self.issues,"risk":self.risk,"reasons":self.reasons,
            "leaves_computer":self.leaves_computer,"commands":self.ops,
        }))
    }
}

impl fmt::Debug for Checked {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.value().fmt(f)
    }
}

impl Serialize for Checked {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        self.value().serialize(serializer)
    }
}

/// Sequential overlays let later steps refer to effects of earlier steps.
#[derive(Clone, Debug)]
pub struct PlanState {
    pub dataset_slot: Option<String>,
    pub dataset_planned: bool,
    pub origin: Origin,
    pub actor_id: Option<String>,
    /// Optional ownership facts prevent conservative CLI classification
    /// from raising the risk before a run can identify its own work.
    pub run: Option<RunFacts>,
    pub destinations: Vec<DestinationInfo>,
    pub connected: BTreeSet<String>,
    pub streaming: BTreeMap<String, bool>,
    pub forwarding: BTreeSet<String>,
    /// None denotes a recording proposed by an earlier step; live forwards
    /// without a worker-issued identity are deliberately absent.
    pub forwarding_ids: BTreeMap<String, Option<String>>,
    pub jobs: BTreeMap<String, usize>,
    pub canceled_jobs: BTreeSet<(String, u64)>,
    pub changed_features: BTreeMap<String, BTreeSet<String>>,
    pub planned_bytes: u64,
    pub planned_files: u64,
}

impl PlanState {
    pub fn new(ctx: &Context) -> Self {
        Self {
            dataset_slot: ctx
                .snapshot
                .dataset
                .as_ref()
                .and_then(|dataset| dataset.slot_id.clone()),
            dataset_planned: false,
            origin: Origin::Cli,
            actor_id: None,
            run: None,
            destinations: ctx.destinations.clone(),
            connected: ctx
                .snapshot
                .cameras
                .iter()
                .map(|c| c.info.id.clone())
                .collect(),
            streaming: ctx
                .snapshot
                .cameras
                .iter()
                .map(|c| (c.info.id.clone(), c.streaming))
                .collect(),
            forwarding: ctx
                .snapshot
                .cameras
                .iter()
                .filter(|c| c.forwarding.is_some())
                .map(|c| c.info.id.clone())
                .collect(),
            forwarding_ids: ctx
                .snapshot
                .cameras
                .iter()
                .filter_map(|camera| {
                    camera.forwarding.as_ref()?;
                    Some((camera.info.id.clone(), Some(camera.forwarding_id.clone()?)))
                })
                .collect(),
            jobs: ctx
                .snapshot
                .cameras
                .iter()
                .map(|c| {
                    (
                        c.info.id.clone(),
                        c.jobs.iter().filter(|j| j.active()).count(),
                    )
                })
                .collect(),
            canceled_jobs: BTreeSet::new(),
            changed_features: BTreeMap::new(),
            planned_bytes: 0,
            planned_files: 0,
        }
    }

    pub fn assistant(ctx: &Context) -> Self {
        Self {
            origin: Origin::Assistant,
            ..Self::new(ctx)
        }
    }

    pub fn resolve_camera(
        &self,
        ctx: &Context,
        field: &str,
        text: &str,
    ) -> std::result::Result<CameraRef, Issue> {
        if let Some(id) = self
            .connected
            .iter()
            .find(|id| id.eq_ignore_ascii_case(text.trim()))
        {
            return Ok(CameraRef::Connected(id.clone()));
        }
        let camera = super::resolve_camera(ctx, field, text, self.origin == Origin::Assistant)?;
        let selector = camera.selector();
        let userinfo = selector.split_once("://").is_some_and(|(_, rest)| {
            rest.split(['/', '?', '#'])
                .next()
                .is_some_and(|authority| authority.contains('@'))
        });
        if self.origin == Origin::Assistant
            && camera.connected().is_none()
            && (userinfo || redact::for_output_text(selector) != selector)
        {
            return Err(Issue::usage(
                Some(field),
                "Remove the password from the URL. Connect password-protected streams outside the assistant.",
            ));
        }
        Ok(camera)
    }

    pub fn is_forwarding(&self, camera: &str) -> bool {
        self.forwarding.contains(camera)
    }
    pub fn is_streaming(&self, camera: &str) -> bool {
        self.streaming.get(camera).copied().unwrap_or(false)
    }
    pub fn active_jobs(&self, camera: &str) -> usize {
        self.jobs.get(camera).copied().unwrap_or(0)
    }
}

/// Facts owned by one autonomous run. Actor attribution is conservative:
/// missing ownership information never grants permission to stop work.
#[derive(Clone, Debug, Default)]
pub struct RunFacts {
    pub actor_id: String,
    pub scope: BTreeSet<String>,
    pub tokens: Vec<(String, String)>,
    pub jobs: BTreeSet<(String, u64)>,
    pub forwarders: BTreeSet<String>,
    /// A reconnect can reuse a camera ID and job number. Ownership ends
    /// when its worker lifetime ends.
    pub worker_pids: BTreeMap<String, u32>,
    pub human_values: BTreeMap<(String, String), String>,
    /// The exact open dataset slot lifetime authorized for this task.
    pub dataset: Option<String>,
}

fn raise(checked: &mut Checked, reason: &str) {
    checked.risk = checked.risk.max(Risk::Disruptive);
    let reason = redact::for_output_text(reason);
    if !checked.reasons.contains(&reason) {
        checked.reasons.push(reason);
    }
}

/// Raise the catalog risk when an action could interrupt somebody else's
/// work or overwrite a setting they changed.
pub fn classify(ctx: &Context, run: Option<&RunFacts>, checked: &mut Checked) {
    if super::spec(&checked.call.tool).is_some_and(|s| s.kind == ToolKind::Read) {
        return;
    }
    let tool = checked.call.tool.clone();
    let camera = checked.call.args["camera"].as_str().map(str::to_owned);
    let actor = run
        .map(|r| r.actor_id.as_str())
        .or(checked.actor_id.as_deref())
        .map(str::to_owned);
    if tool == "stream_to_url" {
        raise(checked, "Sends video to another computer.");
    }
    if tool == "run_camera_command" {
        let command = checked.call.args["command"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        if !super::risk::command_allowed(&command) && !super::risk::acquisition_command(&command) {
            raise(
                checked,
                "Runs a command outside the image-quality allowlist.",
            );
        }
    }
    let Some(camera) = camera else {
        return;
    };
    if tool == "undo_camera_change"
        && let Some(token) = checked.call.args["token"].as_str()
        && !run.is_some_and(|run| {
            run.tokens
                .iter()
                .any(|(id, owned)| id == &camera && owned == token)
        })
    {
        raise(
            checked,
            "This change token has not been recorded as this run's own change.",
        );
    }
    let state = ctx.snapshot.cameras.iter().find(|c| c.info.id == camera);
    let busy = state.is_some_and(|c| c.forwarding.is_some() || c.jobs.iter().any(|j| j.active()));
    if busy
        && (tool == "stop_live_view"
            || checked
                .ops
                .iter()
                .any(|op| matches!(op, Op::Restartable { .. })))
    {
        raise(
            checked,
            "Would interrupt an active recording, stream or schedule.",
        );
    }
    if run.is_some_and(|run| !run.scope.contains(&camera))
        && matches!(
            tool.as_str(),
            "set_auto_exposure" | "connect_camera" | "disconnect_camera"
        )
    {
        raise(checked, "Changes a camera outside this task's scope.");
    }
    let scanner = state
        .map(|c| &c.info)
        .or_else(|| ctx.snapshot.devices.iter().find(|c| c.id == camera))
        .is_some_and(|info| info.transport == crate::types::Transport::Scanner);
    if scanner
        && matches!(
            tool.as_str(),
            "connect_camera"
                | "start_live_view"
                | "capture_photos"
                | "schedule_captures"
                | "start_recording"
        )
    {
        raise(checked, "Starts a physical scanner.");
    }
    let activity = || {
        ctx.snapshot
            .activity
            .iter()
            .filter(|a| a.camera.as_deref() == Some(&camera))
    };
    let owns =
        |a: &crate::session::SessionActivity| actor.as_deref().is_some_and(|id| a.actor.id == id);
    let foreign_recent_write = activity()
        .filter(|a| {
            !owns(a)
                && a.actor.kind != crate::session::ActorKind::Agent
                && a.state != crate::session::ActivityState::Failed
                && matches!(
                    a.operation.as_str(),
                    "set"
                        | "set_many"
                        | "apply_setup"
                        | "execute"
                        | "auto"
                        | "manual"
                        | "replay_camera_change"
                        | "undo"
                        | "redo"
                )
        })
        .max_by_key(|a| a.id)
        .is_some_and(|a| {
            ctx.now_ms.saturating_sub(
                a.finished_at_ms
                    .or(a.started_at_ms)
                    .unwrap_or(a.submitted_at_ms),
            ) <= 600_000
        });
    if foreign_recent_write {
        raise(checked, "A person recently changed this camera.");
    }
    let run_owns_lifetime = run.is_some_and(|run| {
        state.is_some_and(|state| run.worker_pids.get(&camera) == Some(&state.worker_pid))
    });
    // Timestamps and actor labels cannot prove which forwarding instance is
    // current. The run supplies only identities it still owns exactly.
    let own_forwarder = run_owns_lifetime && run.is_some_and(|r| r.forwarders.contains(&camera));
    let foreign_jobs = state.is_some_and(|c| {
        c.jobs.iter().filter(|j| j.active()).any(|j| {
            !(run_owns_lifetime && run.is_some_and(|r| r.jobs.contains(&(camera.clone(), j.id))))
                && !activity().any(|a| {
                    a.job
                        .as_ref()
                        .is_some_and(|job| job.id == j.id && job.worker_pid == c.worker_pid)
                        && owns(a)
                })
        })
    });
    if state.is_some_and(|c| c.forwarding.is_some()) && !own_forwarder || foreign_jobs {
        raise(
            checked,
            "This camera has work started by another participant.",
        );
    }
    if tool == "stop_recording" && !own_forwarder {
        raise(
            checked,
            "Stops a recording or stream this caller did not start.",
        );
    }
    if tool == "cancel_job" {
        let id = checked.call.args["job_id"].as_u64().unwrap_or_default();
        let own = run_owns_lifetime && run.is_some_and(|r| r.jobs.contains(&(camera.clone(), id)))
            || activity().any(|a| {
                a.job.as_ref().is_some_and(|j| {
                    j.id == id && state.is_some_and(|state| j.worker_pid == state.worker_pid)
                }) && owns(a)
            });
        if !own {
            raise(checked, "Cancels a schedule this caller did not start.");
        }
    }
    let features: Vec<String> = if tool == "set_camera_features" {
        checked.call.args["values"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v["feature"].as_str().map(str::to_owned))
            .collect()
    } else if tool == "set_camera_roi" {
        ["Width", "Height", "OffsetX", "OffsetY"]
            .map(str::to_owned)
            .to_vec()
    } else {
        Vec::new()
    };
    // Auto controls and one-shot commands can change values indirectly.
    // A person's writes remain protected for the whole task, including after
    // the recent-activity window and when an auto policy releases a control.
    let indirect_auto = tool == "set_auto_exposure"
        || tool == "run_camera_command"
            && checked.call.args["command"].as_str().is_some_and(|name| {
                name.ends_with("AutoOnce") || name.starts_with("Auto") && name.ends_with("Once")
            })
        || features.iter().any(|name| name.ends_with("Auto"))
        || state
            .and_then(|camera| camera.auto.as_ref())
            .is_some_and(|auto| {
                features
                    .iter()
                    .any(|feature| auto.managed.contains(feature))
            });
    if indirect_auto && let Some(run) = run {
        for (id, feature) in run.human_values.keys() {
            if id == &camera && super::risk::image_quality(feature) {
                raise(
                    checked,
                    &format!("May overwrite {feature}, which a person changed during this task."),
                );
            }
        }
    }
    for feature in features {
        if !super::risk::image_quality(&feature) {
            raise(
                checked,
                "Writes a feature outside the image-quality allowlist.",
            );
        }
        if run.is_some_and(|r| {
            r.human_values
                .contains_key(&(camera.clone(), feature.clone()))
        }) {
            raise(
                checked,
                &format!("Overwrites {feature}, which a person changed during this task."),
            );
        }
    }
}

/// Secrets stay outside calls, checked plans and execution reports.
pub trait SecretSource {
    fn secret(&self, step: StepId, field: &str) -> Option<Zeroizing<String>>;
}

pub struct EmptySecrets;
impl SecretSource for EmptySecrets {
    fn secret(&self, _step: StepId, _field: &str) -> Option<Zeroizing<String>> {
        None
    }
}
impl<F> SecretSource for F
where
    F: Fn(StepId, &str) -> Option<Zeroizing<String>>,
{
    fn secret(&self, step: StepId, field: &str) -> Option<Zeroizing<String>> {
        self(step, field)
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct StepOutcome {
    pub summary: String,
    pub results: Vec<Value>,
    pub watch: Vec<Value>,
}

/// One executor per plan or run preserves connect aliases and undo tokens.
pub struct Executor<'a> {
    dataset_slot: Option<String>,
    host: &'a dyn ToolHost,
    secrets: &'a dyn SecretSource,
    pub aliases: HashMap<String, String>,
    pub tokens: Vec<(String, String)>,
    acknowledged: Vec<Value>,
    forwarders: BTreeMap<String, String>,
}

impl<'a> Executor<'a> {
    pub fn new(host: &'a dyn ToolHost, secrets: &'a dyn SecretSource) -> Self {
        Self {
            dataset_slot: None,
            host,
            secrets,
            aliases: HashMap::new(),
            tokens: Vec::new(),
            acknowledged: Vec::new(),
            forwarders: BTreeMap::new(),
        }
    }

    /// Acknowledged camera commands from the latest step remain available
    /// when a later restart or verification fails. Forward acknowledgements
    /// retain only their opaque identity, never stream command results.
    pub fn acknowledged(&self) -> &[Value] {
        &self.acknowledged
    }

    fn camera(&self, camera: &CameraRef) -> Result<String> {
        if let Some(id) = self.aliases.get(camera.selector()) {
            return Ok(id.clone());
        }
        match camera {
            CameraRef::Connected(id) => Ok(id.clone()),
            _ => bail!(
                "Connect {} before running this camera command.",
                redact::for_output_text(camera.selector())
            ),
        }
    }

    fn command(&mut self, camera: &CameraRef, command: SessionCommand) -> Result<Value> {
        command.validate()?;
        let camera = self.camera(camera)?;
        let forward = matches!(command, SessionCommand::Forward { .. });
        let stops_forward = matches!(
            command,
            SessionCommand::StopForwardIf { .. }
                | SessionCommand::StopForward
                | SessionCommand::Stop
                | SessionCommand::Disconnect
        );
        let changes = match &command {
            SessionCommand::SetMany { values } => values
                .iter()
                .map(|v| v.feature.clone())
                .collect::<BTreeSet<_>>(),
            _ => BTreeSet::new(),
        };
        let duration_ms = match &command {
            SessionCommand::Forward { duration_ms, .. } => *duration_ms,
            _ => None,
        };
        let started = Instant::now();
        let before = if forward || !changes.is_empty() {
            Some(self.host.snapshot()?)
        } else {
            None
        };
        let previous: Vec<_> = before
            .as_ref()
            .into_iter()
            .flat_map(|snapshot| snapshot.cameras.iter().find(|c| c.info.id == camera))
            .flat_map(|c| c.features.iter())
            .filter(|feature| changes.contains(feature.name.as_str()))
            .map(|feature| json!({"name":feature.name,"value":feature.value}))
            .collect();
        let mut result = self.host.execute(Some(&camera), command)?;
        if !previous.is_empty() && result.is_object() {
            result["before"] = json!(previous);
            result["after"] = result["values"].clone();
        }
        if let Some(token) = result["camera_change"]["token"]
            .as_str()
            .or_else(|| result["camera_change"].as_str())
        {
            self.tokens.push((camera.clone(), token.into()));
        }
        if forward {
            if let Some(id) = result["forwarding_id"]
                .as_str()
                .filter(|id| crate::session::valid_forwarding_id(id))
            {
                self.acknowledged.push(json!({"forwarding_id":id}));
                self.forwarders.insert(camera.clone(), id.into());
            }
        } else {
            self.acknowledged.push(redact::for_output(result.clone()));
        }
        if let Some(before) = before.filter(|_| forward) {
            self.verify_forward(&camera, &before, duration_ms, started, &result)?;
        }
        if stops_forward {
            self.forwarders.remove(&camera);
        }
        Ok(redact::for_output(result))
    }

    fn verify_forward(
        &self,
        camera: &str,
        before: &crate::session::SessionSnapshot,
        duration_ms: Option<u64>,
        started: Instant,
        reply: &Value,
    ) -> Result<()> {
        let initial = before.cameras.iter().find(|c| c.info.id == camera);
        let initial_error = initial.and_then(|c| c.last_error.as_deref());
        let initial_logged = crate::session::camera_error_logged_ms(&before.logs, camera);
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut seen = !reply["forwarding"].is_null();
        let until_ms = reply["until_ms"].as_u64();
        loop {
            let snapshot = self.host.snapshot()?;
            let state = snapshot.cameras.iter().find(|c| c.info.id == camera);
            let error = state.and_then(|c| c.last_error.as_deref());
            let logged = crate::session::camera_error_logged_ms(&snapshot.logs, camera);
            if error != initial_error || logged > initial_logged {
                bail!(
                    "The recording or stream failed to start: {}",
                    redact::for_output_text(error.unwrap_or("the camera reported an error"))
                );
            }
            let forwarding = state.is_some_and(|c| c.forwarding.is_some());
            if forwarding && let Some(id) = reply["forwarding_id"].as_str() {
                ensure!(
                    state.and_then(|state| state.forwarding_id.as_deref()) == Some(id),
                    "The recording or stream changed while it was starting."
                );
            }
            let reached_deadline = duration_ms
                .is_some_and(|duration| started.elapsed().as_millis() >= u128::from(duration))
                || until_ms.is_some_and(|until| {
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .is_ok_and(|now| now.as_millis() >= u128::from(until))
                });
            if seen && !forwarding && reached_deadline {
                return Ok(());
            }
            ensure!(
                !seen || forwarding,
                "The recording or stream stopped while it was starting."
            );
            seen |= forwarding;
            if Instant::now() >= deadline {
                ensure!(
                    seen,
                    "The camera did not confirm that recording or streaming started."
                );
                return Ok(());
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    pub fn run_step(&mut self, checked: &Checked, cancel: &AtomicBool) -> Result<StepOutcome> {
        self.acknowledged.clear();
        ensure!(
            checked.ready(),
            "Review the errors in this step before running it."
        );
        let mut results = Vec::new();
        let mut watch = Vec::new();
        for op in &checked.ops {
            ensure!(
                !cancel.load(Ordering::Relaxed),
                "The camera task was stopped."
            );
            if let Some(root) = &checked.output_root {
                let command = match op {
                    Op::Command { command, .. } | Op::Stream { command, .. } => Some(command),
                    _ => None,
                };
                if let Some(
                    SessionCommand::Capture {
                        output,
                        destination,
                        ..
                    }
                    | SessionCommand::Schedule {
                        output,
                        destination,
                        ..
                    }
                    | SessionCommand::Forward {
                        output,
                        destination,
                        ..
                    },
                ) = command
                    && destination.is_none()
                    && !output.contains("://")
                {
                    super::values::contain_path(root, &PathBuf::from(output))
                        .map_err(|issue| anyhow!(issue.message))?;
                }
            }
            let result = match op {
                Op::Session {
                    command,
                    dataset_from_plan,
                } => {
                    let mut command = command.clone();
                    if *dataset_from_plan {
                        let slot = self.dataset_slot.as_deref().context("The earlier step did not acknowledge its dataset. Review the dataset before continuing.")?;
                        super::data::bind_dataset(&mut command, slot)?;
                    }
                    super::data::contain_output(checked.output_root.as_deref(), &command)?;
                    command.validate()?;
                    let result = self.host.execute(None, command)?;
                    super::data::acknowledge(&result, &mut self.dataset_slot);
                    self.acknowledged.push(redact::for_output(result.clone()));
                    super::read::bound(result)
                }
                Op::Connect {
                    selector,
                    saved_login,
                } => {
                    let command = SessionCommand::Connect {
                        camera: selector.clone(),
                        timeout_ms: 10_000,
                        login: None,
                        saved_login: *saved_login,
                    };
                    command.validate()?;
                    let result = self.host.execute(None, command).map_err(|error| {
                        if !*saved_login && crate::logins::is_auth_error(&error) {
                            anyhow::anyhow!(
                                "This camera needs a sign-in. Connect it in the app or with `capturefab connect`; the assistant can then use the connected camera."
                            )
                        } else {
                            error
                        }
                    })?;
                    let id = result["connected"]["id"]
                        .as_str()
                        .context("The connection did not return a camera ID.")?;
                    self.aliases.insert(selector.clone(), id.into());
                    if let Some(original) = checked.call.args["camera"].as_str() {
                        self.aliases.insert(original.into(), id.into());
                    }
                    if let Some((base, _)) = selector.split_once('?') {
                        self.aliases.insert(base.into(), id.into());
                    }
                    result
                }
                Op::Command { camera, command } => self.command(camera, command.clone())?,
                Op::StopForward {
                    camera,
                    forwarding_id,
                } => {
                    let id = self.camera(camera)?;
                    let forwarding_id = forwarding_id
                        .clone()
                        .or_else(|| self.forwarders.get(&id).cloned());
                    let forwarding_id = forwarding_id.context("The earlier step did not identify its recording. Review the camera before stopping it.")?;
                    self.command(camera, SessionCommand::StopForwardIf { forwarding_id })?
                }
                Op::Stream { camera, command } => {
                    let mut command = command.clone();
                    let password = self.secrets.secret(checked.id, "password");
                    if let SessionCommand::Forward { output, .. } = &mut command
                        && let Some(secret) = password.as_ref()
                    {
                        *output = merge_stream_password(output, secret)?;
                    }
                    // Error strings may quote the credential-bearing output.
                    let known = serde_json::to_value(&command)?;
                    let mut result = self.command(camera, command).map_err(|e| {
                        let message = redact::for_output_error(&format!("{e:#}"), &known);
                        anyhow!(password.as_ref().map_or_else(
                            || message.clone(),
                            |secret| hide_secret(&message, secret)
                        ))
                    })?;
                    if let Some(secret) = password.as_ref() {
                        hide_secret_value(&mut result, secret);
                    }
                    result
                }
                Op::Undo { camera, token } => {
                    let id = self.camera(camera)?;
                    let token = token
                        .clone()
                        .or_else(|| {
                            self.tokens
                                .iter()
                                .rev()
                                .find(|(camera, _)| camera == &id)
                                .map(|(_, token)| token.clone())
                        })
                        .context("There is no camera change from this caller to restore.")?;
                    self.command(
                        &CameraRef::Connected(id),
                        SessionCommand::ReplayCameraChange { token, undo: true },
                    )?
                }
                Op::Restartable { camera, commands } => {
                    let mut stopped = false;
                    let mut sequence = Vec::new();
                    let mut failure = None;
                    for command in commands {
                        let start = matches!(command, SessionCommand::Start);
                        if (failure.is_some() || cancel.load(Ordering::Relaxed))
                            && !(stopped && start)
                        {
                            continue;
                        }
                        match self.command(camera, command.clone()) {
                            Ok(value) => {
                                if matches!(command, SessionCommand::Stop) {
                                    stopped = true;
                                }
                                if start {
                                    stopped = false;
                                }
                                sequence.push(value);
                            }
                            Err(error) => {
                                if failure.is_none() {
                                    failure = Some(error);
                                }
                            }
                        }
                    }
                    if stopped {
                        let restored = self.command(camera, SessionCommand::Start);
                        if let Err(error) = restored {
                            return Err(error.context(match failure {
                                Some(previous) => {
                                    format!("Acquisition could not resume after {previous:#}")
                                }
                                None => "Acquisition could not resume.".into(),
                            }));
                        }
                    }
                    if let Some(error) = failure {
                        return Err(error);
                    }
                    ensure!(
                        !cancel.load(Ordering::Relaxed),
                        "The camera task was stopped after acquisition resumed."
                    );
                    json!({"results":sequence})
                }
                Op::CreateDestination { destination } => create_destination(destination)?,
                Op::Observe(call) => {
                    let ctx = Context::gather(
                        self.host,
                        checked
                            .output_root
                            .clone()
                            .unwrap_or_else(|| PathBuf::from(".")),
                        crate::storage::configured_policy()?,
                        super::context::RecordingDefaults::default(),
                    )?;
                    super::read::run_read(
                        self.host,
                        &ctx,
                        call,
                        &super::read::ReadOptions::new(Origin::Cli, cancel),
                    )
                    .map_err(|issues| {
                        anyhow!(
                            issues
                                .first()
                                .map_or("The observation failed.", |i| &i.message)
                                .to_owned()
                        )
                    })?
                    .value
                }
            };
            let result = redact::for_output(result);
            if result.get("job").is_some() || result.get("until_ms").is_some() {
                watch.push(result.clone());
            }
            results.push(result);
        }
        Ok(StepOutcome {
            summary: redact::for_output_text(&checked.summary),
            results,
            watch,
        })
    }
}

fn merge_stream_password(output: &str, password: &str) -> Result<String> {
    let (scheme, rest) = output
        .split_once("://")
        .context("Enter a complete stream URL.")?;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    ensure!(!authority.is_empty(), "Use a stream URL with a host.");
    let encoded = percent_encode(password);
    match scheme {
        "srt" => {
            let (head, fragment) = output.split_once('#').map_or((output, ""), |(a, b)| (a, b));
            let separator = if head.contains('?') { '&' } else { '?' };
            Ok(format!(
                "{head}{separator}passphrase={encoded}{}",
                if fragment.is_empty() {
                    String::new()
                } else {
                    format!("#{fragment}")
                }
            ))
        }
        "rtmp" | "rtmps" if output.ends_with('/') => Ok(format!("{output}{encoded}")),
        "rtsp" | "rtsps" | "rtmp" | "rtmps" => {
            let (user, host) = authority
                .rsplit_once('@')
                .map_or(("", authority), |(user, host)| (user, host));
            ensure!(
                !user.contains(':'),
                "Remove the password from the URL and enter it in Stream password."
            );
            Ok(format!(
                "{scheme}://{user}:{encoded}@{host}{}",
                &rest[end..]
            ))
        }
        _ => bail!("Use RTSP, RTMP or SRT for a stream password."),
    }
}

fn percent_encode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            write!(out, "%{byte:02X}").expect("write to string");
        }
    }
    out
}

fn hide_secret(text: &str, secret: &str) -> String {
    if secret.is_empty() {
        return text.into();
    }
    text.replace(secret, "<redacted>")
        .replace(&percent_encode(secret), "<redacted>")
}

fn hide_secret_value(value: &mut Value, secret: &str) {
    match value {
        Value::String(text) => *text = hide_secret(text, secret),
        Value::Array(values) => {
            for value in values {
                hide_secret_value(value, secret);
            }
        }
        Value::Object(fields) => {
            for value in fields.values_mut() {
                hide_secret_value(value, secret);
            }
        }
        _ => (),
    }
}

fn url_host(value: &str) -> Option<String> {
    let (_, rest) = value.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    Some(
        authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host)
            .to_ascii_lowercase(),
    )
}

fn create_destination(destination: &Destination) -> Result<Value> {
    destination.validate()?;
    let crate::destination::Target::Folder { path } = &destination.target;
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .context("Choose a folder for captures after configuring your home directory.")?;
    super::values::folder_path_live(
        path,
        &home,
        &crate::ipc::runtime_dir(),
        &crate::ipc::state_dir(),
    )
    .map_err(|issue| anyhow!(issue.message))?;
    crate::destination::create(destination)?;
    Ok(json!({"destination":destination}))
}

#[derive(Debug)]
pub struct CheckFailure {
    pub issues_by_step: Vec<(usize, Vec<Issue>)>,
    pub checked: Value,
}
impl CheckFailure {
    pub fn details(&self) -> Value {
        redact::for_output(
            json!({"issues":self.issues_by_step.iter().flat_map(|(step,issues)| issues.iter().map(move |issue| json!({"step":step,"field":issue.field,"message":issue.message,"severity":issue.severity,"code":issue.code}))).collect::<Vec<_>>(),"checked":self.checked}),
        )
    }
}
impl fmt::Display for CheckFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self
            .issues_by_step
            .iter()
            .find_map(|(step, issues)| issues.iter().find(|i| i.is_error()).map(|i| (step, i)))
        {
            Some((step, issue)) => write!(
                f,
                "step {step}: {}",
                redact::for_output_text(&issue.message)
            ),
            None => f.write_str("Review this plan before running it."),
        }
    }
}
impl std::error::Error for CheckFailure {}

#[derive(Debug)]
pub struct PlanFailure {
    pub source: anyhow::Error,
    pub completed: Vec<Value>,
    pub failed_step: usize,
    pub step_count: usize,
}
impl PlanFailure {
    pub fn details(&self) -> Value {
        redact::for_output(
            json!({"completed":self.completed,"failed_step":self.failed_step,"step_count":self.step_count}),
        )
    }
}
impl fmt::Display for PlanFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "step {} of {}: {}",
            self.failed_step,
            self.step_count,
            redact::for_output_text(&format!("{:#}", self.source))
        )
    }
}
impl std::error::Error for PlanFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

#[derive(Debug)]
pub enum ApplyError {
    Check(CheckFailure),
    Run(PlanFailure),
}
impl fmt::Display for ApplyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Check(e) => e.fmt(f),
            Self::Run(e) => e.fmt(f),
        }
    }
}
impl std::error::Error for ApplyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(match self {
            Self::Check(e) => e,
            Self::Run(e) => e,
        })
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgressState {
    Running,
    Completed,
}
#[derive(Clone, Debug, Serialize)]
pub struct Progress {
    pub step: usize,
    pub step_count: usize,
    pub state: ProgressState,
    pub summary: String,
}

fn capture_facts(checked: &Checked) -> bool {
    matches!(
        checked.call.tool.as_str(),
        "capture_photos" | "start_recording" | "schedule_captures"
    )
}

fn immediate_schedule(checked: &Checked) -> bool {
    checked.call.tool == "schedule_captures" && checked.call.args["start_at"].is_null()
}

/// Compare approval-bearing facts while allowing display-only clocks and
/// available-space meters to advance. Feature before-values and recipients
/// remain exact, including when they happen to use one of these labels.
pub(crate) fn approval_facts(checked: &Checked) -> Vec<(&str, &str)> {
    let capture = capture_facts(checked);
    let dynamic_file = capture
        && checked.call.args["file_name"]
            .as_str()
            .is_some_and(|template| {
                ["{time}", "{date}", "{datetime}"]
                    .iter()
                    .any(|part| template.contains(part))
            });
    checked
        .facts
        .iter()
        .filter_map(|(name, value)| {
            if (name == "File" && dynamic_file)
                || (name == "Schedule" && immediate_schedule(checked))
            {
                None
            } else if name == "Storage" && capture {
                Some((
                    name.as_str(),
                    value
                        .split_once(" · ")
                        .map_or(value.as_str(), |(estimate, _)| estimate),
                ))
            } else {
                Some((name.as_str(), value.as_str()))
            }
        })
        .collect()
}

/// An immediate schedule means start when execution begins. Only that start
/// timestamp may move during approval; count, interval, output, destination and
/// every other command field must still match the checked operations.
pub(crate) fn same_approval_operations(approved: &Checked, fresh: &Checked) -> bool {
    if approved.data_resources != fresh.data_resources {
        return false;
    }
    fn comparable(checked: &Checked) -> Result<Value, serde_json::Error> {
        let mut ops = checked.ops.clone();
        if immediate_schedule(checked) {
            for op in &mut ops {
                if let Op::Command {
                    command: SessionCommand::Schedule { first_at_ms, .. },
                    ..
                } = op
                {
                    *first_at_ms = 0;
                }
            }
        }
        serde_json::to_value(&ops)
    }
    match (comparable(approved), comparable(fresh)) {
        (Ok(approved), Ok(fresh)) => approved == fresh,
        _ => false,
    }
}

pub(crate) fn changed(approved: &Checked, current: &Checked) -> Option<&'static str> {
    if approved.id != current.id || approved.call.tool != current.call.tool {
        return Some("the action");
    }
    if approved.call.args["camera"] != current.call.args["camera"] {
        return Some("the camera");
    }
    if approved.call.args["destination"] != current.call.args["destination"]
        || !same_destination(approved.destination.as_ref(), current.destination.as_ref())
    {
        return Some("the destination");
    }
    if approved.output_root != current.output_root {
        return Some("the output folder");
    }
    let host = |c: &Checked| c.call.args["url"].as_str().and_then(url_host);
    if host(approved) != host(current) {
        return Some("the stream host");
    }
    for field in ["start_at", "until"] {
        let time = |c: &Checked| {
            c.call.args[field]
                .as_str()
                .and_then(|s| crate::scheduling::parse_at(s).ok())
        };
        match (time(approved), time(current)) {
            (Some(a), Some(b)) if a.abs_diff(b) <= 60_000 => (),
            (None, None) if approved.call.args[field] == current.call.args[field] => (),
            _ => return Some("the scheduled time"),
        }
    }
    if approved.risk != current.risk {
        return Some("the risk");
    }
    if approved.issues != current.issues {
        return Some("the warnings or errors");
    }
    if approved.title != current.title
        || approved.summary != current.summary
        || approval_facts(approved) != approval_facts(current)
        || approved.reasons != current.reasons
    {
        return Some("the displayed camera facts");
    }
    // Binding all other arguments prevents changes to feature values,
    // duration, file names or stream paths being silently approved.
    let mut a = approved.call.args.clone();
    let mut b = current.call.args.clone();
    for field in ["start_at", "until"] {
        if let Some(o) = a.as_object_mut() {
            o.remove(field);
        }
        if let Some(o) = b.as_object_mut() {
            o.remove(field);
        }
    }
    if a != b {
        return Some("the arguments");
    }
    if !same_approval_operations(approved, current) {
        return Some("the checked operations");
    }
    None
}

fn same_destination(a: Option<&DestinationInfo>, b: Option<&DestinationInfo>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => {
            a.name == b.name
                && a.kind == b.kind
                && a.location == b.location
                && a.host == b.host
                && a.remote == b.remote
                && a.insecure == b.insecure
                && a.approval_identity == b.approval_identity
        }
        (None, None) => true,
        _ => false,
    }
}

/// Re-gather and check the entire plan before the first effect. The caller
/// passes normalized calls from the approved plan, preserving pinned times.
pub fn apply(
    host: &dyn ToolHost,
    secrets: &dyn SecretSource,
    gather: impl Fn() -> Result<Context>,
    steps: &[(StepId, ToolCall)],
    approved: Option<&[Checked]>,
    mut progress: impl FnMut(Progress),
    cancel: &AtomicBool,
) -> std::result::Result<Value, ApplyError> {
    let mut early_issues = Vec::new();
    if !(1..=super::MAX_PLAN_CALLS).contains(&steps.len()) {
        early_issues.push((
            1,
            vec![Issue::usage(None, "Use a plan with 1 to 64 steps.")],
        ));
    }
    let mut ids = BTreeSet::new();
    for (index, (id, _)) in steps.iter().enumerate() {
        if !ids.insert(*id) {
            early_issues.push((
                index + 1,
                vec![Issue::usage(None, "Give each step its own stable ID.")],
            ));
        }
    }
    if !early_issues.is_empty() {
        return Err(ApplyError::Check(CheckFailure {
            issues_by_step: early_issues,
            checked: json!([]),
        }));
    }
    let ctx = gather().map_err(|source| {
        ApplyError::Run(PlanFailure {
            source,
            completed: Vec::new(),
            failed_step: 1,
            step_count: steps.len(),
        })
    })?;
    let present = |id, field: &str| secrets.secret(id, field).is_some();
    let mut state = PlanState::new(&ctx);
    state.actor_id = host.actor_id();
    let checked = super::check::check_plan_with(&ctx, &mut state, steps, &present);
    if let Some(approved) = approved {
        if approved.len() != checked.len() {
            return Err(ApplyError::Check(CheckFailure {
                issues_by_step: vec![(
                    1,
                    vec![Issue::usage(
                        None,
                        "The plan changed since you approved it. Review the plan again.",
                    )],
                )],
                checked: json!(checked),
            }));
        }
        for (index, (before, after)) in approved.iter().zip(&checked).enumerate() {
            if let Some(what) = changed(before, after) {
                return Err(ApplyError::Check(CheckFailure {
                    issues_by_step: vec![(
                        index + 1,
                        vec![Issue::usage(
                            None,
                            format!(
                                "Step {} changed since you approved it: {what}. Review the plan again.",
                                index + 1
                            ),
                        )],
                    )],
                    checked: json!(checked),
                }));
            }
        }
    }
    let issues_by_step: Vec<_> = checked
        .iter()
        .enumerate()
        .filter(|(_, step)| !step.ready())
        .map(|(index, step)| (index + 1, step.issues.clone()))
        .collect();
    if !issues_by_step.is_empty() {
        return Err(ApplyError::Check(CheckFailure {
            issues_by_step,
            checked: json!(checked),
        }));
    }
    let mut executor = Executor::new(host, secrets);
    let mut completed = Vec::new();
    for (index, step) in checked.iter().enumerate() {
        // Earlier steps can take minutes. Resolve the saved name again
        // immediately before use so a concurrent editor cannot redirect
        // a later capture to another folder or server.
        if let Some(destination) = step.destination.as_ref()
            && super::spec(&step.call.tool).is_some_and(|spec| spec.kind != ToolKind::Configure)
        {
            let fresh = gather().map_err(|source| {
                ApplyError::Run(PlanFailure {
                    source,
                    completed: completed.clone(),
                    failed_step: index + 1,
                    step_count: checked.len(),
                })
            })?;
            if !same_destination(
                Some(destination),
                fresh
                    .destinations
                    .iter()
                    .find(|d| d.name == destination.name),
            ) {
                return Err(ApplyError::Run(PlanFailure {
                    source: anyhow!(
                        "Step {} changed since you approved it: the destination. Review the plan again.",
                        index + 1
                    ),
                    completed,
                    failed_step: index + 1,
                    step_count: checked.len(),
                }));
            }
        }
        progress(Progress {
            step: index + 1,
            step_count: checked.len(),
            state: ProgressState::Running,
            summary: redact::for_output_text(&step.summary),
        });
        match executor.run_step(step, cancel) {
            Ok(outcome) => {
                completed.push(json!(outcome));
                progress(Progress {
                    step: index + 1,
                    step_count: checked.len(),
                    state: ProgressState::Completed,
                    summary: redact::for_output_text(&step.summary),
                });
            }
            Err(source) => {
                return Err(ApplyError::Run(PlanFailure {
                    source: anyhow!(redact::for_output_error(
                        &format!("{source:#}"),
                        &step.call.args
                    )),
                    completed,
                    failed_step: index + 1,
                    step_count: checked.len(),
                }));
            }
        }
    }
    Ok(redact::for_output(json!({"completed":completed})))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{ActivityState, FeatureValue};
    use crate::tools::tests::{FakeHost, SCANNER, fixture_context, fixture_snapshot};

    fn step(tool: &str, args: Value) -> Checked {
        Checked::new(
            StepId(12),
            &ToolCall {
                tool: tool.into(),
                args,
            },
        )
    }

    #[test]
    fn explicit_undo_needs_approval_unless_the_run_owns_the_token() {
        let mut ctx = fixture_context(fixture_snapshot());
        ctx.snapshot.activity.clear();
        for camera in &mut ctx.snapshot.cameras {
            camera.jobs.clear();
            camera.forwarding = None;
        }
        let mut checked = step(
            "undo_camera_change",
            json!({"camera":"sim:0","token":"change-one"}),
        );
        checked.risk = Risk::Reversible;
        classify(&ctx, None, &mut checked);
        assert_eq!(checked.risk, Risk::Disruptive);
        let facts = RunFacts {
            scope: BTreeSet::from(["sim:0".into()]),
            tokens: vec![("sim:0".into(), "change-one".into())],
            ..RunFacts::default()
        };
        let mut owned = step(
            "undo_camera_change",
            json!({"camera":"sim:0","token":"change-one"}),
        );
        owned.risk = Risk::Reversible;
        classify(&ctx, Some(&facts), &mut owned);
        assert_eq!(owned.risk, Risk::Reversible);
    }

    #[test]
    fn changed_displayed_facts_invalidate_the_exact_plan_approval() {
        let mut approved = step("set_camera_features", json!({"camera":"sim:0"}));
        approved.facts = vec![("Gain".into(), "1 → 3".into())];
        let mut fresh = approved.clone();
        fresh.facts[0].1 = "2 → 3".into();
        assert_eq!(
            changed(&approved, &fresh),
            Some("the displayed camera facts")
        );
    }

    #[test]
    fn approval_facts_allow_capture_clock_and_free_space_drift_but_keep_security_facts() {
        for tool in ["capture_photos", "start_recording", "schedule_captures"] {
            let mut approved = step(
                tool,
                json!({"camera":"sim:0","file_name":"shot-{datetime}.png","start_at":null}),
            );
            approved.facts = vec![
                ("File".into(), "/captures/shot-20261018-120000.png".into()),
                ("Storage".into(), "≈ 8 MiB · 2 GiB of 10 GiB free".into()),
                ("Folder".into(), "/captures".into()),
            ];
            let mut fresh = approved.clone();
            fresh.facts[0].1 = "/captures/shot-20261018-120100.png".into();
            fresh.facts[1].1 = "≈ 8 MiB · 1 GiB of 10 GiB free".into();
            assert_eq!(changed(&approved, &fresh), None, "{tool}");
            fresh.facts[1].1 = "≈ 16 MiB · 1 GiB of 10 GiB free".into();
            assert_eq!(
                changed(&approved, &fresh),
                Some("the displayed camera facts")
            );
            fresh = approved.clone();
            fresh.facts[2].1 = "/another-recipient".into();
            assert_eq!(
                changed(&approved, &fresh),
                Some("the displayed camera facts")
            );
            approved.call.args["file_name"] = json!("fixed.png");
            fresh = approved.clone();
            fresh.facts[0].1 = "/captures/different.png".into();
            assert_eq!(
                changed(&approved, &fresh),
                Some("the displayed camera facts")
            );
        }
        let mut approved = step(
            "set_camera_features",
            json!({"camera":"sim:0","file_name":"{time}"}),
        );
        approved.facts = vec![
            ("File".into(), "1 → 3".into()),
            ("Storage".into(), "1 → 3 · feature unit".into()),
        ];
        let mut fresh = approved.clone();
        fresh.facts[0].1 = "2 → 3".into();
        assert!(changed(&approved, &fresh).is_some());
        fresh = approved.clone();
        fresh.facts[1].1 = "1 → 3 · changed feature unit".into();
        assert!(changed(&approved, &fresh).is_some());
    }

    fn checked_capture(ctx: &Context, tool: &str) -> Checked {
        let schema = (super::super::spec(tool).unwrap().schema)();
        let mut args = Value::Object(
            schema["properties"]
                .as_object()
                .unwrap()
                .keys()
                .map(|name| (name.clone(), Value::Null))
                .collect(),
        );
        args["camera"] = json!("sim:0");
        args["file_name"] = json!("shot-{datetime}.png");
        if tool == "schedule_captures" {
            args["every"] = json!("1m");
            args["count"] = json!(3);
        }
        let call = ToolCall {
            tool: tool.into(),
            args,
        };
        super::super::check(ctx, &mut PlanState::new(ctx), StepId(12), &call, &|_, _| {
            false
        })
    }

    #[test]
    fn an_immediate_schedule_approval_binds_every_operation_except_its_execution_start() {
        let ctx = fixture_context(fixture_snapshot());
        let approved = checked_capture(&ctx, "schedule_captures");
        assert!(approved.ready(), "{approved:?}");
        let mut later = ctx.clone();
        later.now_ms += 61_000;
        later.storage.allocated_bytes += 1024 * 1024;
        let fresh = checked_capture(&later, "schedule_captures");
        assert!(fresh.ready(), "{fresh:?}");
        assert_ne!(approved.facts, fresh.facts);
        assert_eq!(changed(&approved, &fresh), None);
        assert!(same_approval_operations(&approved, &fresh));
        let mut altered = fresh.clone();
        for op in &mut altered.ops {
            if let Op::Command {
                command: SessionCommand::Schedule { count, .. },
                ..
            } = op
            {
                *count += 1;
            }
        }
        assert!(!same_approval_operations(&approved, &altered));
        let mut explicit = approved.clone();
        explicit.call.args["start_at"] = json!("2026-10-18T12:00:00Z");
        let mut explicit_later = fresh.clone();
        explicit_later.call.args["start_at"] = explicit.call.args["start_at"].clone();
        assert!(!same_approval_operations(&explicit, &explicit_later));
        assert_ne!(approval_facts(&explicit), approval_facts(&explicit_later));
    }

    #[test]
    fn a_storage_retention_change_invalidates_unchanged_displayed_capture_facts() {
        let ctx = fixture_context(fixture_snapshot());
        let approved = checked_capture(&ctx, "capture_photos");
        assert!(approved.ready(), "{approved:?}");
        let mut fresh_context = ctx.clone();
        fresh_context.storage.request.max_age_seconds = Some(1);
        let fresh = checked_capture(&fresh_context, "capture_photos");
        assert_eq!(approved.facts, fresh.facts);
        assert_eq!(approved.call, fresh.call);
        assert_eq!(changed(&approved, &fresh), Some("the checked operations"));
    }

    #[test]
    fn a_saved_destination_path_change_is_a_different_approval_target() {
        let ctx = fixture_context(fixture_snapshot());
        let mut approved = checked_capture(&ctx, "capture_photos");
        let mut destination = ctx.destinations[1].clone();
        destination.approval_identity = Some("first-folder-identity".into());
        approved.destination = Some(destination);
        let mut fresh = approved.clone();
        fresh.destination.as_mut().unwrap().approval_identity =
            Some("second-folder-identity".into());
        assert_eq!(
            serde_json::to_value(&approved.destination).unwrap(),
            serde_json::to_value(&fresh.destination).unwrap()
        );
        assert_eq!(changed(&approved, &fresh), Some("the destination"));
    }

    fn forward(output: &str) -> SessionCommand {
        SessionCommand::Forward {
            output: output.into(),
            codec: "h264".into(),
            encoder: "auto".into(),
            fps: 30.0,
            bitrate: "4M".into(),
            storage: crate::storage::StoragePolicy::default(),
            max_file_bytes: crate::media::default_recording_cap(),
            destination: None,
            on_exists: crate::naming::OnExists::Number,
            context: Default::default(),
            duration_ms: None,
        }
    }

    #[test]
    fn executor_tracks_connect_aliases_and_pins_every_command() {
        let host = FakeHost::new(fixture_snapshot());
        let mut executor = Executor::new(&host, &EmptySecrets);
        let mut checked = step(
            "connect_camera",
            json!({"camera":"avfoundation:Test Camera"}),
        );
        checked.ops = vec![
            Op::Connect {
                selector: "avfoundation:Test Camera?size=640x480&fps=30".into(),
                saved_login: true,
            },
            Op::Command {
                camera: CameraRef::Connected("avfoundation:Test Camera".into()),
                command: SessionCommand::Start,
            },
        ];
        executor
            .run_step(&checked, &AtomicBool::new(false))
            .unwrap();
        let sent = host.sent();
        assert_eq!(sent[0].0, None);
        assert_eq!(sent[1].0.as_deref(), Some("media:00000000000000aa"));
        assert_eq!(sent[1].1["op"], "start");
        let mut unconnected = step("start_live_view", json!({"camera":"sim:99"}));
        unconnected.ops.push(Op::Command {
            camera: CameraRef::Typed("sim:99".into()),
            command: SessionCommand::Start,
        });
        assert!(
            executor
                .run_step(&unconnected, &AtomicBool::new(false))
                .unwrap_err()
                .to_string()
                .contains("Connect")
        );
        assert_eq!(host.calls().len(), 2);
    }

    #[test]
    fn executor_remembers_camera_change_tokens_for_undo() {
        let host = FakeHost::new(fixture_snapshot()).reply(
            "set_many",
            Ok(json!({"values":[{"name":"Gain","value":2}],"camera_change":{"token":"worker:1:change:2"}})),
        );
        let mut executor = Executor::new(&host, &EmptySecrets);
        let camera = CameraRef::Connected("sim:0".into());
        let mut checked = step("set_camera_features", json!({"camera":"sim:0"}));
        checked.ops.push(Op::Command {
            camera: camera.clone(),
            command: SessionCommand::SetMany {
                values: vec![FeatureValue {
                    feature: "Gain".into(),
                    value: "2".into(),
                }],
            },
        });
        let outcome = executor
            .run_step(&checked, &AtomicBool::new(false))
            .unwrap();
        assert_eq!(
            outcome.results[0]["before"],
            json!([{"name":"Gain","value":0.0}])
        );
        assert_eq!(
            outcome.results[0]["after"],
            json!([{"name":"Gain","value":2}])
        );
        checked.ops = vec![Op::Undo {
            camera,
            token: None,
        }];
        executor
            .run_step(&checked, &AtomicBool::new(false))
            .unwrap();
        let sent = host.sent();
        assert_eq!(sent[1].1["token"], "worker:1:change:2");
        assert_eq!(sent[1].1["undo"], true);
        assert!(
            sent.iter()
                .all(|(camera, _)| camera.as_deref() == Some("sim:0"))
        );
    }

    struct CancelAfterStop<'a> {
        host: FakeHost,
        cancel: &'a AtomicBool,
    }
    impl ToolHost for CancelAfterStop<'_> {
        fn snapshot(&self) -> Result<crate::session::SessionSnapshot> {
            self.host.snapshot()
        }
        fn execute(&self, camera: Option<&str>, command: SessionCommand) -> Result<Value> {
            let stop = matches!(command, SessionCommand::Stop);
            let result = self.host.execute(camera, command);
            if stop {
                self.cancel.store(true, Ordering::Relaxed);
            }
            result
        }
        fn has_session(&self) -> bool {
            true
        }
    }

    fn restart_step() -> Checked {
        let mut checked = step("set_camera_features", json!({"camera":"sim:0"}));
        checked.ops.push(Op::Restartable {
            camera: CameraRef::Connected("sim:0".into()),
            commands: vec![
                SessionCommand::Stop,
                SessionCommand::SetMany {
                    values: vec![FeatureValue {
                        feature: "Width".into(),
                        value: "800".into(),
                    }],
                },
                SessionCommand::Start,
            ],
        });
        checked
    }

    #[test]
    fn restart_resumes_after_cancellation_or_a_failed_write() {
        let untouched = FakeHost::new(fixture_snapshot());
        assert!(
            Executor::new(&untouched, &EmptySecrets)
                .run_step(&restart_step(), &AtomicBool::new(true))
                .is_err()
        );
        assert!(untouched.calls().is_empty());
        let cancel = AtomicBool::new(false);
        let host = CancelAfterStop {
            host: FakeHost::new(fixture_snapshot()),
            cancel: &cancel,
        };
        let mut executor = Executor::new(&host, &EmptySecrets);
        assert!(executor.run_step(&restart_step(), &cancel).is_err());
        assert_eq!(host.host.ops(), ["stop", "start"]);
        let host =
            FakeHost::new(fixture_snapshot()).reply("set_many", Err("write conflict".into()));
        let mut executor = Executor::new(&host, &EmptySecrets);
        assert!(
            executor
                .run_step(&restart_step(), &AtomicBool::new(false))
                .unwrap_err()
                .to_string()
                .contains("write conflict")
        );
        assert_eq!(host.ops(), ["stop", "set_many", "start"]);
    }

    #[test]
    fn stream_passwords_merge_only_in_the_sent_command_and_never_the_report() {
        for (url, expected) in [
            ("rtsp://alice@h/path", "rtsp://alice:p%40ss%2Fword@h/path"),
            (
                "srt://h:9000?mode=caller",
                "srt://h:9000?mode=caller&passphrase=p%40ss%2Fword",
            ),
            ("rtmps://h/live/", "rtmps://h/live/p%40ss%2Fword"),
        ] {
            assert_eq!(merge_stream_password(url, "p@ss/word").unwrap(), expected);
        }
        let mut snapshot = fixture_snapshot();
        snapshot
            .cameras
            .iter_mut()
            .find(|c| c.info.id == "sim:0")
            .unwrap()
            .forwarding = Some("rtmps://h/live/secret".into());
        let host = FakeHost::new(snapshot)
            .reply("forward", Ok(json!({"diagnostic":"private-stream-key"})));
        let secrets = |id: StepId, field: &str| {
            (id == StepId(12) && field == "password")
                .then(|| Zeroizing::new("private-stream-key".into()))
        };
        let mut executor = Executor::new(&host, &secrets);
        let mut checked = step(
            "stream_to_url",
            json!({"camera":"sim:0","url":"rtmps://h/live/"}),
        );
        checked.ops.push(Op::Stream {
            camera: CameraRef::Connected("sim:0".into()),
            command: forward("rtmps://h/live/"),
        });
        let outcome = executor
            .run_step(&checked, &AtomicBool::new(false))
            .unwrap();
        assert_eq!(
            host.sent()[0].1["output"],
            "rtmps://h/live/private-stream-key"
        );
        assert!(
            !serde_json::to_string(&outcome)
                .unwrap()
                .contains("private-stream-key")
        );
        assert!(
            !serde_json::to_string(&checked)
                .unwrap()
                .contains("private-stream-key")
        );
        assert!(!format!("{checked:?}").contains("private-stream-key"));
        let host = FakeHost::new(fixture_snapshot())
            .reply("forward", Err("private-stream-key was refused".into()));
        let error = Executor::new(&host, &secrets)
            .run_step(&checked, &AtomicBool::new(false))
            .unwrap_err();
        assert!(!error.to_string().contains("private-stream-key"));
        assert!(error.to_string().contains("<redacted>"));
    }

    #[test]
    fn forward_verification_accepts_a_short_recording_that_reaches_its_deadline() {
        let before = fixture_snapshot();
        let mut running = before.clone();
        running
            .cameras
            .iter_mut()
            .find(|c| c.info.id == "sim:0")
            .unwrap()
            .forwarding = Some("short.mp4".into());
        let host = FakeHost::new(running).then(before.clone());
        let executor = Executor::new(&host, &EmptySecrets);
        executor
            .verify_forward(
                "sim:0",
                &before,
                Some(1000),
                Instant::now() - Duration::from_secs(2),
                &json!({}),
            )
            .unwrap();
    }

    #[test]
    fn forward_verification_accepts_a_delayed_start_reply_after_a_clean_short_recording() {
        let host = FakeHost::new(fixture_snapshot()).reply(
            "forward",
            Ok(json!({"forwarding":"rtmps://h/live/short","since_ms":0,"until_ms":1})),
        );
        let mut command = forward("rtmps://h/live/short");
        if let SessionCommand::Forward { duration_ms, .. } = &mut command {
            *duration_ms = Some(1000);
        }
        let mut checked = step("start_recording", json!({"camera":"sim:0"}));
        checked.ops.push(Op::Command {
            camera: CameraRef::Connected("sim:0".into()),
            command,
        });
        Executor::new(&host, &EmptySecrets)
            .run_step(&checked, &AtomicBool::new(false))
            .unwrap();
        assert_eq!(host.ops(), ["forward"]);
    }

    #[test]
    fn forward_verification_accepts_elapsed_duration_after_wall_clock_rollback() {
        let before = fixture_snapshot();
        let host = FakeHost::new(before.clone());
        Executor::new(&host, &EmptySecrets)
            .verify_forward(
                "sim:0",
                &before,
                Some(1_000),
                Instant::now() - Duration::from_secs(2),
                &json!({"forwarding":"short.mp4","until_ms":u64::MAX}),
            )
            .unwrap();
    }

    #[test]
    fn forward_verification_observes_a_late_startup_error() {
        let mut before = fixture_snapshot();
        let camera = before
            .cameras
            .iter_mut()
            .find(|c| c.info.id == "sim:0")
            .unwrap();
        camera.forwarding = None;
        let mut running = before.clone();
        running
            .cameras
            .iter_mut()
            .find(|c| c.info.id == "sim:0")
            .unwrap()
            .forwarding = Some("rtmps://h/live/x".into());
        let mut failed = running.clone();
        let camera = failed
            .cameras
            .iter_mut()
            .find(|c| c.info.id == "sim:0")
            .unwrap();
        camera.forwarding = None;
        camera.last_error = Some("encoder failed".into());
        let host = FakeHost::new(before).then(running).then(failed);
        let mut executor = Executor::new(&host, &EmptySecrets);
        let mut checked = step("stream_to_url", json!({"camera":"sim:0"}));
        checked.ops.push(Op::Command {
            camera: CameraRef::Connected("sim:0".into()),
            command: forward("rtmps://h/live/x"),
        });
        assert!(
            executor
                .run_step(&checked, &AtomicBool::new(false))
                .unwrap_err()
                .to_string()
                .contains("encoder failed")
        );
    }

    #[test]
    fn accepted_forward_identity_survives_a_failed_verification_without_stream_secrets() {
        struct LostStatus(FakeHost, std::sync::atomic::AtomicUsize);
        impl ToolHost for LostStatus {
            fn snapshot(&self) -> Result<crate::session::SessionSnapshot> {
                ensure!(
                    self.1.fetch_add(1, Ordering::Relaxed) != 1,
                    "status temporarily unavailable"
                );
                self.0.snapshot()
            }
            fn execute(&self, camera: Option<&str>, command: SessionCommand) -> Result<Value> {
                self.0.execute(camera, command)
            }
            fn has_session(&self) -> bool {
                true
            }
        }
        let id = "0123456789abcdef0123456789abcdef";
        let host = LostStatus(
            FakeHost::new(fixture_snapshot()).reply(
                "forward",
                Ok(
                    json!({"forwarding_id":id,"forwarding":"rtmps://h/live/private-key",
                          "diagnostic":"private-key"}),
                ),
            ),
            std::sync::atomic::AtomicUsize::new(0),
        );
        let mut executor = Executor::new(&host, &EmptySecrets);
        let mut checked = step("stream_to_url", json!({"camera":"sim:0"}));
        checked.ops.push(Op::Command {
            camera: CameraRef::Connected("sim:0".into()),
            command: forward("rtmps://h/live/private-key"),
        });
        assert!(
            executor
                .run_step(&checked, &AtomicBool::new(false))
                .is_err()
        );
        assert_eq!(executor.acknowledged(), &[json!({"forwarding_id":id})]);
        assert_eq!(
            executor.forwarders.get("sim:0").map(String::as_str),
            Some(id)
        );
        checked.ops = vec![Op::StopForward {
            camera: CameraRef::Connected("sim:0".into()),
            forwarding_id: None,
        }];
        executor
            .run_step(&checked, &AtomicBool::new(false))
            .unwrap();
        assert_eq!(host.0.sent()[1].1["forwarding_id"], id);
    }

    #[test]
    fn approval_binding_detects_camera_times_and_argument_changes() {
        let before = step(
            "schedule_captures",
            json!({"camera":"sim:0","start_at":"2026-10-18T12:30:00Z","until":null,"count":10}),
        );
        let mut after = before.clone();
        after.call.args["camera"] = json!("sim:1");
        assert_eq!(changed(&before, &after), Some("the camera"));
        after = before.clone();
        after.call.args["start_at"] = json!("2026-10-19T12:30:00Z");
        assert_eq!(changed(&before, &after), Some("the scheduled time"));
        after = before.clone();
        after.call.args["start_at"] = json!("2026-10-18T12:30:30Z");
        assert_eq!(changed(&before, &after), None);
        after.call.args["count"] = json!(100);
        assert_eq!(changed(&before, &after), Some("the arguments"));
        after = before.clone();
        after.destination = Some(fixture_context(fixture_snapshot()).destinations.remove(0));
        let mut approved = after.clone();
        approved.destination.as_mut().unwrap().free_bytes = Some(1);
        assert_eq!(changed(&approved, &after), None);
        after.destination.as_mut().unwrap().location = "/different".into();
        assert_eq!(changed(&approved, &after), Some("the destination"));
    }

    #[test]
    fn apply_refuses_a_camera_changed_after_approval_without_running() {
        let ctx = fixture_context(fixture_snapshot());
        let call = ToolCall {
            tool: "start_live_view".into(),
            args: json!({"camera":"sim:0"}),
        };
        let mut approved =
            super::super::check_plan(&ctx, &[(StepId(1), call.clone())], &|_, _| false);
        approved[0].call.args["camera"] = json!("sim:1");
        let host = FakeHost::new(ctx.snapshot.clone());
        let error = apply(
            &host,
            &EmptySecrets,
            || Ok(ctx.clone()),
            &[(StepId(1), call)],
            Some(&approved),
            |_| {},
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert!(error.to_string().contains("the camera"));
        assert!(host.calls().is_empty());
    }

    #[test]
    fn apply_refuses_empty_oversized_and_duplicate_steps_before_gather() {
        let host = FakeHost::new(fixture_snapshot());
        let call = ToolCall {
            tool: "start_live_view".into(),
            args: json!({"camera":"sim:0"}),
        };
        for steps in [
            Vec::new(),
            vec![(StepId(1), call.clone()); super::super::MAX_PLAN_CALLS + 1],
            vec![(StepId(1), call.clone()), (StepId(1), call.clone())],
        ] {
            let error = apply(
                &host,
                &EmptySecrets,
                || panic!("must not gather an invalid plan"),
                &steps,
                None,
                |_| {},
                &AtomicBool::new(false),
            )
            .unwrap_err();
            assert!(matches!(error, ApplyError::Check(_)));
        }
        assert!(host.calls().is_empty());
    }

    #[test]
    fn apply_rechecks_a_later_destinations_location_before_using_it() {
        let ctx = fixture_context(fixture_snapshot());
        let host = FakeHost::new(ctx.snapshot.clone());
        let steps = vec![
            (
                StepId(1),
                ToolCall {
                    tool: "start_live_view".into(),
                    args: json!({"camera":"sim:0"}),
                },
            ),
            (
                StepId(2),
                ToolCall {
                    tool: "capture_photos".into(),
                    args: json!({"camera":"sim:0","count":1,"format":"png","file_name":"test.png","destination":"line-1","tags":null}),
                },
            ),
        ];
        let gathers = std::cell::Cell::new(0);
        let error = apply(
            &host,
            &EmptySecrets,
            || {
                let mut next = ctx.clone();
                let count = gathers.get();
                gathers.set(count + 1);
                if count > 0 {
                    next.destinations[0].location = "~/Other".into();
                }
                Ok(next)
            },
            &steps,
            None,
            |_| {},
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert!(error.to_string().contains("the destination"), "{error}");
        assert_eq!(host.ops(), ["start"]);
        let ApplyError::Run(failure) = error else {
            panic!("an earlier step completed")
        };
        assert_eq!(failure.completed.len(), 1);
        assert_eq!(failure.failed_step, 2);
    }

    #[test]
    fn run_ownership_does_not_survive_a_camera_worker_replacement() {
        let mut ctx = fixture_context(fixture_snapshot());
        ctx.snapshot.activity.clear();
        let live = ctx
            .snapshot
            .cameras
            .iter()
            .find(|c| c.info.id == "sim:1")
            .unwrap()
            .worker_pid;
        let mut run = RunFacts::default();
        run.forwarders.insert("sim:1".into());
        run.worker_pids.insert("sim:1".into(), live);
        let mut own = step("stop_recording", json!({"camera":"sim:1"}));
        classify(&ctx, Some(&run), &mut own);
        assert_eq!(own.risk, Risk::Bounded);
        run.worker_pids
            .insert("sim:1".into(), live.saturating_add(1));
        let mut replaced = step("stop_recording", json!({"camera":"sim:1"}));
        classify(&ctx, Some(&run), &mut replaced);
        assert_eq!(replaced.risk, Risk::Disruptive);
    }

    #[test]
    fn pinned_recording_stop_is_conditional_even_after_a_replacement_race() {
        let first = "11111111111111111111111111111111";
        let host = FakeHost::new(fixture_snapshot()).reply(
            "stop_forward_if",
            Err("the recording changed; the current recording was preserved".into()),
        );
        let mut checked = step("stop_recording", json!({"camera":"sim:1"}));
        checked.ops.push(Op::StopForward {
            camera: CameraRef::Connected("sim:1".into()),
            forwarding_id: Some(first.into()),
        });
        let mut executor = Executor::new(&host, &EmptySecrets);
        let error = executor
            .run_step(&checked, &AtomicBool::new(false))
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("current recording was preserved")
        );
        assert_eq!(host.ops(), ["stop_forward_if"]);
        assert_eq!(host.sent()[0].1["forwarding_id"], first);
    }

    #[test]
    fn unpinned_stop_uses_only_this_executors_forward_acknowledgement() {
        let id = "33333333333333333333333333333333";
        let host = FakeHost::new(fixture_snapshot()).reply("forward", Ok(json!({"forwarding":"udp://127.0.0.1:9","forwarding_id":id,"since_ms":0,"until_ms":1})));
        let mut executor = Executor::new(&host, &EmptySecrets);
        let mut stop = step("stop_recording", json!({"camera":"sim:0"}));
        stop.ops.push(Op::StopForward {
            camera: CameraRef::Connected("sim:0".into()),
            forwarding_id: None,
        });
        assert!(executor.run_step(&stop, &AtomicBool::new(false)).is_err());
        assert!(
            host.ops().is_empty(),
            "an unresolved earlier recording cannot stop current work"
        );
        let mut start = step("start_recording", json!({"camera":"sim:0"}));
        start.ops.push(Op::Command {
            camera: CameraRef::Connected("sim:0".into()),
            command: forward("udp://127.0.0.1:9"),
        });
        executor.run_step(&start, &AtomicBool::new(false)).unwrap();
        executor.run_step(&stop, &AtomicBool::new(false)).unwrap();
        assert_eq!(host.ops(), ["forward", "stop_forward_if"]);
        assert_eq!(host.sent()[1].1["forwarding_id"], id);
        assert!(executor.run_step(&stop, &AtomicBool::new(false)).is_err());
        assert_eq!(
            host.ops().len(),
            2,
            "a completed stop consumes its earlier acknowledgement"
        );
    }

    #[test]
    fn indirect_auto_writes_keep_human_values_protected_without_recent_activity() {
        let mut ctx = fixture_context(fixture_snapshot());
        ctx.snapshot.activity.clear();
        for camera in &mut ctx.snapshot.cameras {
            camera.jobs.clear();
            camera.forwarding = None;
        }
        let mut run = RunFacts {
            actor_id: "assistant:1".into(),
            ..RunFacts::default()
        };
        run.scope.insert("sim:0".into());
        run.human_values
            .insert(("sim:0".into(), "ExposureTime".into()), "5000".into());
        for (tool, args) in [
            ("set_auto_exposure", json!({"camera":"sim:0","mode":"auto"})),
            (
                "set_auto_exposure",
                json!({"camera":"sim:0","mode":"revert"}),
            ),
            (
                "run_camera_command",
                json!({"camera":"sim:0","command":"ExposureAutoOnce"}),
            ),
            (
                "set_camera_features",
                json!({"camera":"sim:0","values":[{"feature":"ExposureAuto","value":"Continuous","expect":null}]}),
            ),
        ] {
            let mut checked = step(tool, args);
            checked.risk = Risk::Reversible;
            classify(&ctx, Some(&run), &mut checked);
            assert_eq!(checked.risk, Risk::Disruptive, "{tool}");
            assert!(
                checked
                    .reasons
                    .iter()
                    .any(|reason| reason.contains("person changed")),
                "{tool}"
            );
        }
        let mut trigger = step(
            "run_camera_command",
            json!({"camera":"sim:0","command":"TriggerSoftware"}),
        );
        trigger.risk = Risk::Reversible;
        classify(&ctx, Some(&run), &mut trigger);
        assert_eq!(trigger.risk, Risk::Reversible);
    }

    #[test]
    fn classify_covers_busy_foreign_scanners_scope_and_human_values() {
        let mut ctx = fixture_context(fixture_snapshot());
        ctx.snapshot.activity.clear();
        let mut checked = step("stop_live_view", json!({"camera":"sim:1"}));
        classify(&ctx, None, &mut checked);
        assert_eq!(checked.risk, Risk::Disruptive);
        for (tool, args) in [
            ("capture_photos", json!({"camera":SCANNER})),
            ("stream_to_url", json!({"camera":"sim:0"})),
            (
                "set_camera_features",
                json!({"camera":"sim:0","values":[{"feature":"UndocumentedMode","value":"1","expect":null}]}),
            ),
            (
                "run_camera_command",
                json!({"camera":"sim:0","command":"DangerousCommand"}),
            ),
        ] {
            let mut checked = step(tool, args);
            classify(&ctx, None, &mut checked);
            assert_eq!(checked.risk, Risk::Disruptive, "{tool}");
        }
        let mut run = RunFacts {
            actor_id: "assistant:1".into(),
            ..RunFacts::default()
        };
        let mut checked = step("set_auto_exposure", json!({"camera":"sim:0"}));
        classify(&ctx, Some(&run), &mut checked);
        assert_eq!(checked.risk, Risk::Disruptive);
        run.scope.insert("sim:0".into());
        run.human_values
            .insert(("sim:0".into(), "Gain".into()), "3".into());
        let mut checked = step(
            "set_camera_features",
            json!({"camera":"sim:0","values":[{"feature":"Gain","value":"2","expect":null}]}),
        );
        classify(&ctx, Some(&run), &mut checked);
        assert!(
            checked
                .reasons
                .iter()
                .any(|reason| reason.contains("person changed"))
        );
        let mut ctx = fixture_context(fixture_snapshot());
        ctx.snapshot.activity.push(crate::session::SessionActivity {
            id: 100,
            origin: crate::session::ActivityOrigin::Gui,
            actor: crate::session::SessionActor::new(
                "desktop-test",
                "Desktop",
                crate::session::ActorKind::Human,
            )
            .unwrap(),
            operation: "set".into(),
            camera: Some("sim:0".into()),
            feature: Some("Gain".into()),
            label: "Set Gain".into(),
            state: ActivityState::Succeeded,
            submitted_at_ms: ctx.now_ms,
            started_at_ms: Some(ctx.now_ms),
            finished_at_ms: Some(ctx.now_ms),
            detail: None,
            file_count: 0,
            files: Vec::new(),
            job: None,
        });
        let mut checked = step("start_live_view", json!({"camera":"sim:0"}));
        classify(&ctx, None, &mut checked);
        assert!(
            checked
                .reasons
                .iter()
                .any(|reason| reason.contains("recently changed"))
        );
        let mut newer_read = ctx.snapshot.activity.last().unwrap().clone();
        newer_read.id += 1;
        newer_read.operation = "capture".into();
        ctx.snapshot.activity.push(newer_read);
        let mut after_read = step("start_live_view", json!({"camera":"sim:0"}));
        classify(&ctx, None, &mut after_read);
        assert!(
            after_read
                .reasons
                .iter()
                .any(|reason| reason.contains("recently changed")),
            "A later capture must not conceal the person's feature write."
        );
    }
}
