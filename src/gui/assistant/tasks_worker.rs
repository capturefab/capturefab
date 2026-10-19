//! Task work stays on named threads. UI state owns cancellation before any
//! credentials, environment, provider or action-log initialization can begin.
use super::{settings::SecretDraft, sheet::SentImage};
use crate::{
    assistant::{
        self as ai, Settings, agent,
        approval::{self, ApprovalDecision, ApprovalRequest, Approver},
        run::{self, RunControl, RunEvent, RunOutcome, RunRequest},
    },
    session::{ActivityOrigin, SessionActor, SessionHandle},
    storage::StoragePolicy,
    tools::{
        self, Checked, SecretSource, StepId, ToolCall,
        context::{Context, RecordingDefaults, SessionHost},
    },
};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
    time::Duration,
};

#[derive(Clone, Debug)]
pub(in crate::gui) struct Failure {
    pub message: String,
    pub excerpt: Option<String>,
}
impl Failure {
    fn new(error: anyhow::Error) -> Self {
        let excerpt = error
            .downcast_ref::<ai::ProviderError>()
            .and_then(|e| e.details()["provider_message"].as_str().map(str::to_owned));
        Self {
            message: tools::redact::for_output_text(&error.to_string()),
            excerpt,
        }
    }
}

#[derive(Default)]
pub(in crate::gui) struct Secrets(Mutex<BTreeMap<(StepId, String), SecretDraft>>);
impl Secrets {
    pub(super) fn put(&self, step: StepId, value: SecretDraft) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert((step, "secret".into()), value);
    }
    pub(super) fn remove(&self, step: StepId) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&(step, "secret".into()));
    }
    pub(super) fn clear(&self) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
}
impl SecretSource for Secrets {
    fn secret(&self, step: StepId, field: &str) -> Option<zeroize::Zeroizing<String>> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(step, field.into()))
            .map(|s| zeroize::Zeroizing::new(s.as_str().to_owned()))
    }
}

/// Every answer is bound to the request's run and ID. The runtime performs a
/// fresh checked-action comparison after this human decision.
pub(in crate::gui) struct GuiApprover {
    channel: Arc<approval::ChannelApprover>,
    pub(super) secrets: Arc<Secrets>,
}
impl GuiApprover {
    pub(super) fn new() -> (Arc<Self>, Receiver<ApprovalRequest>) {
        let (channel, requests) = approval::ChannelApprover::new();
        (
            Arc::new(Self {
                channel,
                secrets: Arc::new(Secrets::default()),
            }),
            requests,
        )
    }
    pub(super) fn respond(
        &self,
        request: &ApprovalRequest,
        answer: ApprovalDecision,
        secret: SecretDraft,
    ) -> bool {
        if matches!(answer, ApprovalDecision::Approved { .. }) && request.needs_secret {
            self.secrets.put(request.step, secret);
        }
        let sent = self.channel.respond(&request.run, request.id, answer);
        if !sent {
            self.secrets.remove(request.step);
        }
        sent
    }
}
impl Approver for GuiApprover {
    fn request(
        &self,
        request: ApprovalRequest,
        cancel: &AtomicBool,
        timeout: Duration,
    ) -> ApprovalDecision {
        self.secrets.clear();
        self.channel.request(request, cancel, timeout)
    }
}

pub(in crate::gui) trait Services: Send + Sync {
    fn retained_datasets(&self) -> anyhow::Result<Vec<RetainedDataset>> {
        Ok(Vec::new())
    }
    fn human_actor(&self) -> SessionActor {
        SessionActor::process(ActivityOrigin::Gui)
    }
    fn gather(&self) -> anyhow::Result<Context>;
    fn run(
        &self,
        settings: Settings,
        request: RunRequest,
        control: &RunControl,
        approver: Arc<GuiApprover>,
        events: &mut dyn FnMut(RunEvent),
    ) -> anyhow::Result<RunOutcome>;
    fn apply(
        &self,
        calls: &[(StepId, ToolCall)],
        approved: &[Checked],
        guard: &RecoveryGuard,
        cancel: &AtomicBool,
    ) -> Result<Value, tools::ApplyError>;
    fn runs(&self) -> anyhow::Result<Vec<ai::log::RunSummary>>;
    fn read_log(&self, id: &str) -> anyhow::Result<Vec<Value>>;
    fn export_log(&self, id: &str) -> anyhow::Result<Option<PathBuf>> {
        let records = self.read_log(id)?;
        let Some(path) = rfd::FileDialog::new()
            .set_title("Export assistant action log")
            .set_file_name(format!("{id}.jsonl"))
            .save_file()
        else {
            return Ok(None);
        };
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&path)?;
        use std::io::Write;
        for record in records {
            serde_json::to_writer(&mut file, &record)?;
            file.write_all(b"\n")?;
        }
        file.sync_all()?;
        Ok(Some(path))
    }
}

pub(in crate::gui) struct NativeServices {
    handle: SessionHandle,
    actor: SessionActor,
    root: PathBuf,
    policy: StoragePolicy,
    defaults: RecordingDefaults,
}
impl NativeServices {
    pub(in crate::gui) fn new(
        handle: SessionHandle,
        actor: SessionActor,
        root: PathBuf,
        policy: StoragePolicy,
        defaults: RecordingDefaults,
    ) -> Self {
        Self {
            handle,
            actor,
            root,
            policy,
            defaults,
        }
    }
    fn host(&self) -> SessionHost {
        SessionHost::new(self.handle.clone(), ActivityOrigin::Gui, self.actor.clone())
    }
}
impl Services for NativeServices {
    fn retained_datasets(&self) -> anyhow::Result<Vec<RetainedDataset>> {
        let live = self.handle.retained_task_data();
        let mut rows = Vec::new();
        for handle in live {
            let status = handle.status();
            let path = PathBuf::from(&status.catalog.dataset.path);
            let error = retained_issue(&status);
            rows.push(RetainedDataset {
                key: retained_key(),
                path: path.parent().unwrap_or(&path).to_owned(),
                cap: status.catalog.extents.limit,
                detail: format!(
                    "{} · {} stored samples",
                    status.catalog.dataset.name, status.catalog.dataset.samples
                ),
                owner: Some(RetainedOwner::Live(handle)),
                error,
            });
        }
        for row in crate::storage::recover_task_namespaces()? {
            if rows.iter().any(|live| live.path == row.path) {
                continue;
            }
            rows.push(RetainedDataset {
                key: retained_key(),
                path: row.path,
                cap: row.cap,
                detail: format!("Recovered workspace {}", row.id),
                owner: row.handle.map(RetainedOwner::Recovered),
                error: row.error,
            });
        }
        Ok(rows)
    }
    fn human_actor(&self) -> SessionActor {
        self.actor.clone()
    }
    fn gather(&self) -> anyhow::Result<Context> {
        Context::gather(
            &self.host(),
            self.root.clone(),
            self.policy.clone(),
            self.defaults.clone(),
        )
    }
    fn run(
        &self,
        settings: Settings,
        request: RunRequest,
        control: &RunControl,
        approver: Arc<GuiApprover>,
        events: &mut dyn FnMut(RunEvent),
    ) -> anyhow::Result<RunOutcome> {
        let mut deps = agent::Deps::native(
            Arc::new(self.host()),
            settings,
            self.root.clone(),
            self.policy.clone(),
            self.defaults.clone(),
        );
        deps.secrets = approver.secrets.clone();
        run::run_blocking(request, &deps, approver, control, events)
    }
    fn apply(
        &self,
        calls: &[(StepId, ToolCall)],
        approved: &[Checked],
        guard: &RecoveryGuard,
        cancel: &AtomicBool,
    ) -> Result<Value, tools::ApplyError> {
        tools::apply(
            &self.host(),
            &tools::EmptySecrets,
            || {
                let ctx = self.gather()?;
                guard.validate(&ctx)?;
                Ok(ctx)
            },
            calls,
            Some(approved),
            &mut |_| {},
            cancel,
        )
    }
    fn runs(&self) -> anyhow::Result<Vec<ai::log::RunSummary>> {
        ai::log::runs()
    }
    fn read_log(&self, id: &str) -> anyhow::Result<Vec<Value>> {
        ai::log::read_log(id)
    }
}

pub(in crate::gui) struct Unavailable;
impl Services for Unavailable {
    fn gather(&self) -> anyhow::Result<Context> {
        anyhow::bail!("Open a camera session before starting an assistant task.")
    }
    fn run(
        &self,
        _: Settings,
        _: RunRequest,
        _: &RunControl,
        _: Arc<GuiApprover>,
        _: &mut dyn FnMut(RunEvent),
    ) -> anyhow::Result<RunOutcome> {
        self.gather()?;
        unreachable!()
    }
    fn apply(
        &self,
        _: &[(StepId, ToolCall)],
        _: &[Checked],
        _: &RecoveryGuard,
        _: &AtomicBool,
    ) -> Result<Value, tools::ApplyError> {
        unreachable!("unconfigured task cannot approve recovery")
    }
    fn runs(&self) -> anyhow::Result<Vec<ai::log::RunSummary>> {
        Ok(Vec::new())
    }
    fn read_log(&self, _: &str) -> anyhow::Result<Vec<Value>> {
        anyhow::bail!("Choose an assistant action log.")
    }
}

pub(super) enum Event {
    State(Box<run::RunSnapshot>),
    Changes(Vec<run::SettingChange>),
    Text(String),
    Status(String),
    Tool(ToolCall, Value),
    Image(SentImage),
    Notice(String),
    Finished(Box<RunOutcome>),
    Complete(Result<(), Failure>),
}
pub(super) struct Running {
    pub control: RunControl,
    pub events: Receiver<Event>,
    pub approver: Arc<GuiApprover>,
    pub approvals: Receiver<ApprovalRequest>,
    pub done: Arc<AtomicBool>,
}
pub(super) fn start(
    services: Arc<dyn Services>,
    settings: Settings,
    request: RunRequest,
) -> Result<Running, String> {
    let control = RunControl::default();
    let worker = control.clone();
    let done = Arc::new(AtomicBool::new(false));
    let completed = done.clone();
    let (sender, events) = mpsc::sync_channel(32);
    let (approver, approvals) = GuiApprover::new();
    let approvals_worker = approver.clone();
    std::thread::Builder::new()
        .name("capturefab-gui-task".into())
        .spawn(move || {
            let result = services.run(
                settings,
                request,
                &worker,
                approvals_worker.clone(),
                &mut |event| {
                    let event = match event {
                        RunEvent::State(s) => Event::State(Box::new(s)),
                        RunEvent::Finished(o) => Event::Finished(o),
                        RunEvent::Approval(_) => return,
                        RunEvent::Changes(changes) => Event::Changes(changes),
                        RunEvent::Agent(agent::Event::Text(s)) => Event::Text(s),
                        RunEvent::Agent(agent::Event::Status(s)) => Event::Status(s),
                        RunEvent::Agent(agent::Event::Notice(s)) => Event::Notice(s),
                        RunEvent::Agent(agent::Event::Image(p)) => Event::Image(SentImage::new(p)),
                        RunEvent::Agent(agent::Event::ToolRan { call, output, .. }) => {
                            Event::Tool(call, output)
                        }
                        _ => return,
                    };
                    let _ = sender.send(event);
                },
            );
            approvals_worker.secrets.clear();
            completed.store(true, Ordering::SeqCst);
            let _ = sender.send(Event::Complete(result.map(|_| ()).map_err(Failure::new)));
        })
        .map_err(|_| "Could not start the assistant task worker.".to_owned())?;
    Ok(Running {
        control,
        events,
        approver,
        approvals,
        done,
    })
}

#[derive(Clone)]
enum RetainedOwner {
    Live(crate::session::task_data::TaskDataHandle),
    Recovered(Arc<crate::storage::RecoveredNamespace>),
}

#[derive(Clone)]
pub(in crate::gui) struct RetainedDataset {
    pub key: u64,
    pub path: PathBuf,
    pub cap: u64,
    pub detail: String,
    pub error: Option<String>,
    owner: Option<RetainedOwner>,
}
pub(super) fn retained_key() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn retained_issue(status: &crate::session::task_data::TaskDataStatus) -> Option<String> {
    use crate::session::task_data::TaskDataPhase;
    let mut issues = Vec::new();
    for error in [
        status.error.as_ref(),
        status.catalog.dataset.last_error.as_ref(),
        status.sink.error.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        if !issues.contains(error) {
            issues.push(error.clone());
        }
    }
    if status.sink.dropped_total > 0 {
        issues.push(format!(
            "Task writer reports {} dropped samples.",
            crate::gui::format::grouped(status.sink.dropped_total)
        ));
    }
    if issues.is_empty() {
        match status.phase {
            TaskDataPhase::Open | TaskDataPhase::Closing => {
                issues.push("Workspace is still active or closing; saving a copy and removal are unavailable until it is retained.".into());
            }
            TaskDataPhase::RetainedError => {
                issues.push("Dataset persistence or shutdown needs review. Reveal the workspace or retry removal; its reservation remains charged.".into());
            }
            _ if status.catalog.quarantined || !status.catalog.closed => {
                issues.push("Dataset closure is unconfirmed; saving a copy is unavailable. Its reservation remains charged.".into());
            }
            _ => {}
        }
    }
    (!issues.is_empty()).then(|| issues.join("\n"))
}

impl RetainedDataset {
    fn save_copy(&self) -> anyhow::Result<Option<PathBuf>> {
        let Some(output) = rfd::FileDialog::new()
            .set_title("Save task dataset in a new folder")
            .set_file_name("task-dataset-copy")
            .save_file()
        else {
            return Ok(None);
        };
        self.copy_to(&output)?;
        Ok(Some(output))
    }
    pub(super) fn copy_to(&self, output: &std::path::Path) -> anyhow::Result<()> {
        let policy = crate::storage::configured_policy()?;
        match &self.owner {
            Some(RetainedOwner::Live(handle)) => handle.save_copy(output, &policy),
            Some(RetainedOwner::Recovered(handle)) => handle.save_copy(output, &policy),
            None => anyhow::bail!("Workspace is unavailable; refresh its inventory."),
        }
    }
    pub(super) fn available(&self) -> bool {
        self.owner.is_some()
    }
    pub(super) fn copyable(&self) -> bool {
        match &self.owner {
            Some(RetainedOwner::Recovered(_)) => true,
            Some(RetainedOwner::Live(handle)) => {
                let status = handle.status();
                status.catalog.closed
                    && !status.catalog.quarantined
                    && status.phase != crate::session::task_data::TaskDataPhase::Removed
            }
            None => false,
        }
    }
    pub(super) fn removable(&self) -> bool {
        match &self.owner {
            Some(RetainedOwner::Recovered(_)) => true,
            Some(RetainedOwner::Live(handle)) => matches!(
                handle.status().phase,
                crate::session::task_data::TaskDataPhase::Retained
                    | crate::session::task_data::TaskDataPhase::RetainedError
            ),
            None => false,
        }
    }
    fn reveal(&self) -> anyhow::Result<()> {
        match &self.owner {
            Some(RetainedOwner::Live(handle)) => {
                let status = handle.status();
                anyhow::ensure!(
                    status.phase != crate::session::task_data::TaskDataPhase::Removed,
                    "Dataset was already removed."
                );
                anyhow::ensure!(
                    std::path::Path::new(&status.catalog.dataset.path).parent()
                        == Some(self.path.as_path()),
                    "Workspace identity changed."
                );
                // Native capability checks the held original namespace.
                handle.verify_workspace()?;
            }
            Some(RetainedOwner::Recovered(handle)) => handle.verify()?,
            None => anyhow::bail!("This workspace is unavailable; refresh its inventory."),
        }
        crate::gui::reveal::reveal(&self.path)?;
        Ok(())
    }
    fn remove(&self) -> anyhow::Result<Option<crate::session::task_data::TaskDataStatus>> {
        anyhow::ensure!(
            self.removable(),
            "Workspace is still active or unavailable; removal refused."
        );
        match self.owner.as_ref().expect("removable owner") {
            RetainedOwner::Live(handle) => handle.remove(Duration::from_secs(10)).map(Some),
            RetainedOwner::Recovered(handle) => handle.remove().map(|_| None),
        }
    }
}

pub(super) enum Job {
    Retained,
    CopyRetained(RetainedDataset),
    RevealRetained(RetainedDataset),
    RemoveRetained(RetainedDataset),
    Crash,
    Read(String),
    Export(String),
    Preview(run::RunRecord),
    Cleanup(run::RunRecord, run::Leftover),
    Apply(Vec<(StepId, ToolCall)>, Vec<Checked>, RecoveryGuard),
}
pub(super) enum Reply {
    Retained(Result<Vec<RetainedDataset>, String>),
    Copied(Result<Option<PathBuf>, String>),
    Revealed(Result<(), String>),
    Removed(
        u64,
        Result<Option<Box<crate::session::task_data::TaskDataStatus>>, String>,
    ),
    Crash(Result<Option<ai::log::RunSummary>, String>),
    Read(Result<String, String>),
    Export(Result<Option<PathBuf>, String>),
    Preview(Result<(Box<Context>, run::RestorePlan, RecoveryGuard), String>),
    Apply(Result<Value, String>),
}
pub(super) fn job(services: Arc<dyn Services>, job: Job) -> Result<Receiver<Reply>, String> {
    let (sender, receiver) = mpsc::channel();
    std::thread::Builder::new()
        .name("capturefab-task-review".into())
        .spawn(move || {
            let reply = match job {
                Job::CopyRetained(row) => {
                    Reply::Copied(row.save_copy().map_err(|e| format!("{e:#}")))
                }
                Job::Retained => {
                    Reply::Retained(services.retained_datasets().map_err(|e| format!("{e:#}")))
                }
                Job::RevealRetained(row) => {
                    Reply::Revealed(row.reveal().map_err(|e| format!("{e:#}")))
                }
                Job::RemoveRetained(row) => {
                    let result = row
                        .remove()
                        .map(|status| status.map(Box::new))
                        .map_err(|e| format!("{e:#}"));
                    Reply::Removed(row.key, result)
                }
                Job::Crash => Reply::Crash(
                    services
                        .runs()
                        .map(|runs| runs.into_iter().find(|r| !r.complete && !r.active))
                        .map_err(|e| e.to_string()),
                ),
                Job::Read(id) => Reply::Read(
                    services
                        .read_log(&id)
                        .and_then(|records| {
                            let mut text = String::new();
                            for record in records {
                                text.push_str(&serde_json::to_string(&record)?);
                                text.push('\n');
                            }
                            Ok(text)
                        })
                        .map_err(|e| e.to_string()),
                ),
                Job::Export(id) => {
                    Reply::Export(services.export_log(&id).map_err(|e| e.to_string()))
                }
                Job::Preview(record) => Reply::Preview(
                    services
                        .gather()
                        .map(|ctx| {
                            let plan = run::restore(&record, &ctx);
                            let guard = RecoveryGuard {
                                record,
                                leftover: None,
                                proposed: plan.calls.clone(),
                            };
                            (Box::new(ctx), plan, guard)
                        })
                        .map_err(|e| e.to_string()),
                ),
                Job::Cleanup(record, leftover) => Reply::Preview(
                    services
                        .gather()
                        .and_then(|ctx| {
                            let plan = cleanup_plan(&record, &leftover, &ctx)?;
                            let guard = RecoveryGuard {
                                record,
                                leftover: Some(leftover),
                                proposed: plan.calls.clone(),
                            };
                            Ok((Box::new(ctx), plan, guard))
                        })
                        .map_err(|e| e.to_string()),
                ),
                Job::Apply(calls, checked, guard) => Reply::Apply(
                    services
                        .apply(&calls, &checked, &guard, &AtomicBool::new(false))
                        .map_err(|e| e.to_string()),
                ),
            };
            let _ = sender.send(reply);
        })
        .map_err(|_| "Could not start the task review worker.".to_owned())?;
    Ok(receiver)
}

/// Recovery is a new human proposal, limited to effects still attributable to
/// this exact run and worker lifetime. The executor checks it again on apply.
pub(super) fn cleanup_plan(
    record: &run::RunRecord,
    leftover: &run::Leftover,
    ctx: &Context,
) -> anyhow::Result<run::RestorePlan> {
    if leftover.kind == "signal_capture" {
        let token = record
            .signal_captures
            .get(&leftover.camera)
            .ok_or_else(|| {
                anyhow::anyhow!("This task has no acknowledged ownership of the signal capture.")
            })?;
        anyhow::ensure!(
            ctx.snapshot
                .signals
                .iter()
                .any(|source| source.id == leftover.camera
                    && source.capture_id == *token
                    && source.completed_ms.is_none()),
            "The signal capture finished or was replaced after this task. Review the current source status."
        );
        return Ok(run::RestorePlan {
            calls: vec![ToolCall {
                tool: "stop_signal_capture".into(),
                args: serde_json::json!({"source":leftover.camera}),
            }],
            skipped: Vec::new(),
        });
    }
    let camera = ctx
        .snapshot
        .cameras
        .iter()
        .find(|c| c.info.id == leftover.camera)
        .ok_or_else(|| anyhow::anyhow!("Connect the task camera before reviewing this action."))?;
    anyhow::ensure!(
        record.worker_pids.get(&leftover.camera) == Some(&camera.worker_pid),
        "The camera was reconnected after this task. Review it in the inspector."
    );
    let call = match leftover.kind.as_str() {
        "recording" => {
            let id = record
                .forwarders
                .get(&leftover.camera)
                .ok_or_else(|| anyhow::anyhow!("This task no longer owns the recording."))?;
            anyhow::ensure!(
                camera.forwarding_id.as_ref() == Some(id) && camera.forwarding.is_some(),
                "The recording changed after this task. Review the current recording in the inspector."
            );
            ToolCall {
                tool: "stop_recording".into(),
                args: serde_json::json!({"camera":leftover.camera}),
            }
        }
        "schedule" => {
            let id = leftover
                .job_id
                .ok_or_else(|| anyhow::anyhow!("Choose a schedule from this task."))?;
            anyhow::ensure!(
                camera.jobs.iter().any(|j| j.id == id && j.active()),
                "This schedule is no longer active."
            );
            anyhow::ensure!(
                ctx.snapshot
                    .activity
                    .iter()
                    .any(|a| a.camera.as_ref() == Some(&leftover.camera)
                        && a.actor.as_ref().is_some_and(|a| a.id == record.actor_id)
                        && a.job
                            .as_ref()
                            .is_some_and(|j| j.id == id && j.worker_pid == camera.worker_pid)),
                "The schedule's task ownership could not be verified. Review it in the inspector."
            );
            ToolCall {
                tool: "cancel_job".into(),
                args: serde_json::json!({"camera":leftover.camera,"job_id":id}),
            }
        }
        "auto" => {
            let latest = ctx
                .snapshot
                .activity
                .iter()
                .filter(|a| {
                    a.camera.as_ref() == Some(&leftover.camera)
                        && matches!(a.operation.as_str(), "auto" | "manual")
                        && a.state != crate::session::ActivityState::Failed
                })
                .max_by_key(|a| a.id);
            anyhow::ensure!(
                latest.is_some_and(|a| a.actor.as_ref().is_some_and(|a| a.id == record.actor_id)),
                "Auto exposure changed after this task. Review it in the inspector."
            );
            ToolCall {
                tool: "set_auto_exposure".into(),
                args: serde_json::json!({"camera":leftover.camera,"mode":"hold","balance":null}),
            }
        }
        "settings" => {
            let mut scoped = record.clone();
            scoped
                .settings
                .retain(|setting| setting.camera == leftover.camera);
            return Ok(run::restore(&scoped, ctx));
        }
        _ => anyhow::bail!("Review this task's remaining effects in the camera inspector."),
    };
    Ok(run::RestorePlan {
        calls: vec![call],
        skipped: Vec::new(),
    })
}

#[derive(Clone, Debug)]
pub(in crate::gui) struct RecoveryGuard {
    record: run::RunRecord,
    leftover: Option<run::Leftover>,
    proposed: Vec<ToolCall>,
}
impl RecoveryGuard {
    pub(in crate::gui) fn validate(&self, ctx: &Context) -> anyhow::Result<()> {
        let fresh = if let Some(left) = &self.leftover {
            cleanup_plan(&self.record, left, ctx)?
        } else {
            run::restore(&self.record, ctx)
        };
        anyhow::ensure!(
            fresh.calls == self.proposed,
            "The capture resource changed while this action was being reviewed. Review it again before applying."
        );
        Ok(())
    }
}

#[cfg(test)]
mod retained_tests {
    use super::*;
    use crate::session::task_data::TaskDataPhase;

    #[test]
    fn retained_inventory_preserves_persistence_failures_and_uncertain_closure() {
        let state = crate::gui::assistant::task_scenes::fixture(
            "quick-command-task-separate-done",
            crate::session::SessionSnapshot::default(),
        );
        let mut receipt = state.outcome.unwrap().task_data.unwrap();
        assert!(retained_issue(&receipt).is_none());

        receipt.phase = TaskDataPhase::RetainedError;
        let issue = retained_issue(&receipt).unwrap();
        assert!(issue.contains("persistence or shutdown needs review"));

        receipt.error = Some("Checkpoint was interrupted.".into());
        receipt.catalog.dataset.last_error = receipt.error.clone();
        receipt.sink.error = Some("Storage allowance is full.".into());
        receipt.sink.dropped_total = 2;
        let issue = retained_issue(&receipt).unwrap();
        assert_eq!(issue.matches("Checkpoint was interrupted.").count(), 1);
        assert!(issue.contains("Storage allowance is full."));
        assert!(issue.contains("2 dropped samples"));

        receipt.error = None;
        receipt.catalog.dataset.last_error = None;
        receipt.sink.error = None;
        receipt.sink.dropped_total = 0;
        receipt.phase = TaskDataPhase::Closing;
        receipt.catalog.closed = false;
        assert!(
            retained_issue(&receipt)
                .unwrap()
                .contains("still active or closing")
        );
    }
}
