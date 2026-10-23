//! Blocking assistant work. Each job captures its services, approval and
//! credential snapshot before leaving the UI thread.
use super::sheet::SentImage;
use crate::{
    assistant::{
        self as ai, Settings,
        agent::{self, PlanOutcome, PlanRequest},
    },
    session::{ActivityOrigin, SessionActor, SessionHandle},
    storage::StoragePolicy,
    tools::{
        self, ApplyError, Checked, Progress, SecretSource, StepId, ToolCall,
        context::{Context, RecordingDefaults, SessionHost},
        plan::PlanDocument,
    },
};
use serde_json::Value;
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::AtomicBool,
        mpsc::{self, Receiver},
    },
};

#[derive(Clone, Debug)]
pub(in crate::gui) struct Failure {
    pub message: String,
    pub excerpt: Option<String>,
}
impl Failure {
    fn from_error(error: &anyhow::Error) -> Self {
        match error.downcast_ref::<ai::ProviderError>() {
            Some(provider) => Self {
                message: provider.to_string(),
                excerpt: provider.details()["provider_message"]
                    .as_str()
                    .map(str::to_owned),
            },
            None => Self {
                message: tools::redact::for_output_text(&format!("{error:#}")),
                excerpt: None,
            },
        }
    }
}

pub(in crate::gui) trait Services: Send + Sync {
    fn actor_id(&self) -> Option<String> {
        None
    }
    fn gather(&self) -> anyhow::Result<Context>;
    fn turn(
        &self,
        settings: Settings,
        request: PlanRequest,
        secrets: Arc<dyn SecretSource + Send + Sync>,
        events: &mut dyn FnMut(agent::Event),
        cancel: &AtomicBool,
    ) -> anyhow::Result<PlanOutcome>;
    fn apply(
        &self,
        calls: &[(StepId, ToolCall)],
        approved: &[Checked],
        secrets: &dyn SecretSource,
        progress: &mut dyn FnMut(Progress),
        cancel: &AtomicBool,
    ) -> Result<Value, ApplyError>;
    fn export(&self, document: &PlanDocument) -> Result<Option<PathBuf>, String>;
}

pub(in crate::gui) struct NativeServices {
    handle: SessionHandle,
    actor: SessionActor,
    capture_root: PathBuf,
    policy: StoragePolicy,
    defaults: RecordingDefaults,
}
impl NativeServices {
    pub(in crate::gui) fn new(
        handle: SessionHandle,
        actor: SessionActor,
        capture_root: PathBuf,
        policy: StoragePolicy,
        defaults: RecordingDefaults,
    ) -> Self {
        Self {
            handle,
            actor,
            capture_root,
            policy,
            defaults,
        }
    }
    fn host(&self) -> SessionHost {
        SessionHost::new(self.handle.clone(), ActivityOrigin::Gui, self.actor.clone())
    }
}
impl Services for NativeServices {
    fn actor_id(&self) -> Option<String> {
        Some(self.actor.id.clone())
    }
    fn gather(&self) -> anyhow::Result<Context> {
        Context::gather(
            &self.host(),
            self.capture_root.clone(),
            self.policy.clone(),
            self.defaults.clone(),
        )
    }
    fn turn(
        &self,
        settings: Settings,
        request: PlanRequest,
        secrets: Arc<dyn SecretSource + Send + Sync>,
        events: &mut dyn FnMut(agent::Event),
        cancel: &AtomicBool,
    ) -> anyhow::Result<PlanOutcome> {
        let mut deps = agent::Deps::native(
            Arc::new(self.host()),
            settings,
            self.capture_root.clone(),
            self.policy.clone(),
            self.defaults.clone(),
        );
        deps.secrets = secrets;
        agent::plan_turn(request, &deps, events, cancel)
    }
    fn apply(
        &self,
        calls: &[(StepId, ToolCall)],
        approved: &[Checked],
        secrets: &dyn SecretSource,
        progress: &mut dyn FnMut(Progress),
        cancel: &AtomicBool,
    ) -> Result<Value, ApplyError> {
        tools::apply(
            &self.host(),
            secrets,
            || approved_data_context(self.gather()?, approved),
            calls,
            Some(approved),
            progress,
            cancel,
        )
    }
    fn export(&self, document: &PlanDocument) -> Result<Option<PathBuf>, String> {
        tools::plan::to_json(document)
            .map_err(|error| tools::redact::for_output_text(&error.to_string()))?;
        let Some(path) = rfd::FileDialog::new()
            .set_title("Export assistant plan")
            .set_file_name("capturefab-plan.json")
            .add_filter("Capturefab plan", &["json"])
            .save_file()
        else {
            return Ok(None);
        };
        tools::plan::write(&path, document)
            .map_err(|error| tools::redact::for_output_text(&format!("{error:#}")))?;
        Ok(Some(path))
    }
}

/// Fresh application checks retain dataset consent only for reviewed dataset operations.
fn approved_data_context(mut context: Context, approved: &[Checked]) -> anyhow::Result<Context> {
    let allowed = approved
        .iter()
        .any(|step| tools::data::is_action(&step.call.tool));
    agent::authorize_plan_data(&mut context, allowed);
    Ok(context)
}

pub(super) struct Unavailable;
impl Services for Unavailable {
    fn gather(&self) -> anyhow::Result<Context> {
        anyhow::bail!("Open a camera session before using quick commands.")
    }
    fn turn(
        &self,
        _: Settings,
        _: PlanRequest,
        _: Arc<dyn SecretSource + Send + Sync>,
        _: &mut dyn FnMut(agent::Event),
        _: &AtomicBool,
    ) -> anyhow::Result<PlanOutcome> {
        anyhow::bail!("The assistant worker is not configured.")
    }
    fn apply(
        &self,
        calls: &[(StepId, ToolCall)],
        _: &[Checked],
        _: &dyn SecretSource,
        _: &mut dyn FnMut(Progress),
        _: &AtomicBool,
    ) -> Result<Value, ApplyError> {
        Err(ApplyError::Run(tools::PlanFailure {
            source: anyhow::anyhow!("The assistant worker is not configured."),
            completed: vec![],
            failed_step: 1,
            step_count: calls.len(),
        }))
    }
    fn export(&self, _: &PlanDocument) -> Result<Option<PathBuf>, String> {
        Err("The assistant worker is not configured.".into())
    }
}

pub(super) enum TurnEvent {
    Status(String),
    Text(String),
    Tool {
        call: ToolCall,
        output: Value,
        images: Vec<SentImage>,
    },
    Plan(agent::Plan),
    Usage(ai::Usage),
    Notice(String),
    Image(SentImage),
    Finished(Result<Box<PlanOutcome>, Failure>),
}
pub(super) enum ApplyEvent {
    Progress(Progress),
    Finished {
        result: Result<Value, ApplyError>,
        context: Option<Box<Context>>,
    },
}

fn spawn<T: Send + 'static>(
    name: &str,
    work: impl FnOnce(mpsc::SyncSender<T>) + Send + 'static,
) -> Result<Receiver<T>, String> {
    let (sender, receiver) = mpsc::sync_channel(32);
    std::thread::Builder::new()
        .name(name.into())
        .spawn(move || work(sender))
        .map_err(|_| "Could not start the assistant worker. Try again.".to_owned())?;
    Ok(receiver)
}
pub(super) fn gather(
    services: Arc<dyn Services>,
) -> Result<Receiver<Result<Context, Failure>>, String> {
    spawn("capturefab-assistant-context", move |sender| {
        let result = services
            .gather()
            .map_err(|error| Failure::from_error(&error));
        let _ = sender.send(result);
    })
}
pub(super) fn turn(
    services: Arc<dyn Services>,
    settings: Settings,
    request: PlanRequest,
    secrets: Arc<dyn SecretSource + Send + Sync>,
    cancel: Arc<AtomicBool>,
) -> Result<Receiver<TurnEvent>, String> {
    spawn("capturefab-assistant-plan", move |sender| {
        let mut events = |event| {
            let event = match event {
                agent::Event::Status(value) => TurnEvent::Status(value),
                agent::Event::Text(value) => TurnEvent::Text(value),
                agent::Event::ToolRan {
                    call,
                    output,
                    images,
                } => TurnEvent::Tool {
                    call,
                    output,
                    images: images.into_iter().map(SentImage::new).collect(),
                },
                agent::Event::Plan(value) => TurnEvent::Plan(value),
                agent::Event::Usage(value) => TurnEvent::Usage(value),
                agent::Event::Notice(value) => TurnEvent::Notice(value),
                agent::Event::Image(value) => TurnEvent::Image(SentImage::new(value)),
            };
            let _ = sender.send(event);
        };
        let result = services
            .turn(settings, request, secrets, &mut events, &cancel)
            .map(Box::new)
            .map_err(|error| Failure::from_error(&error));
        let _ = sender.send(TurnEvent::Finished(result));
    })
}
pub(super) fn apply(
    services: Arc<dyn Services>,
    calls: Vec<(StepId, ToolCall)>,
    approved: Vec<Checked>,
    secrets: Arc<dyn SecretSource + Send + Sync>,
    cancel: Arc<AtomicBool>,
) -> Result<Receiver<ApplyEvent>, String> {
    spawn("capturefab-assistant-apply", move |sender| {
        let result = services.apply(
            &calls,
            &approved,
            secrets.as_ref(),
            &mut |value| {
                let _ = sender.send(ApplyEvent::Progress(value));
            },
            &cancel,
        );
        let context = services.gather().ok().map(Box::new);
        let _ = sender.send(ApplyEvent::Finished { result, context });
    })
}
pub(super) fn export(
    services: Arc<dyn Services>,
    document: PlanDocument,
) -> Result<Receiver<Result<Option<PathBuf>, String>>, String> {
    spawn("capturefab-assistant-export", move |sender| {
        let _ = sender.send(services.export(&document));
    })
}
