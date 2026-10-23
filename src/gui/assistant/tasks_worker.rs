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

pub(super) enum Job {
    Crash,
    Read(String),
    Export(String),
    Preview(run::RunRecord),
    Cleanup(run::RunRecord, run::Leftover),
    Apply(Vec<(StepId, ToolCall)>, Vec<Checked>, RecoveryGuard),
}
pub(super) enum Reply {
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
                        && a.actor.id == record.actor_id
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
                latest.is_some_and(|a| a.actor.id == record.actor_id),
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
