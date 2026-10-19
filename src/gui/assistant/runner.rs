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
    fn test_s3(&self, checked: &Checked, secrets: &dyn SecretSource) -> Result<String, String>;
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
    fn test_s3(&self, checked: &Checked, secrets: &dyn SecretSource) -> Result<String, String> {
        #[cfg(feature = "s3")]
        {
            let destination = checked
                .ops
                .iter()
                .find_map(|op| match op {
                    tools::Op::CreateDestination { destination, .. } => Some(destination),
                    _ => None,
                })
                .ok_or_else(|| "Complete this destination's fields before testing.".to_owned())?;
            let crate::destination::Target::S3(bucket) = &destination.target else {
                return Err("Choose an S3 destination to test.".into());
            };
            let client = match &bucket.credentials {
                crate::destination::Credentials::Keychain { .. } => {
                    let key = secrets
                        .secret(checked.id, "secret")
                        .ok_or_else(|| "Enter the secret access key before testing.".to_owned())?;
                    crate::s3::Client::with_secret(bucket, key.to_string())
                }
                _ => crate::s3::Client::new(bucket),
            }
            .map_err(|error| {
                tools::redact::for_output_error(&error.to_string(), &checked.call.args)
            })?;
            match client.check() {
                Ok(()) => Ok("Done · The bucket is available.".into()),
                Err(error) if error.to_string().contains("may not inspect it") => {
                    Ok("The credentials may upload files but cannot inspect the bucket.".into())
                }
                Err(error) => Err(tools::redact::for_output_error(
                    &format!("{error:#}"),
                    &checked.call.args,
                )),
            }
        }
        #[cfg(not(feature = "s3"))]
        {
            let _ = (checked, secrets);
            Err("Use a build with S3 support to test this destination.".into())
        }
    }
}

/// Fresh application checks receive only the exact endpoints already present
/// in the locally reviewed operations. Cached display locations never bind.
fn approved_data_context(mut context: Context, approved: &[Checked]) -> anyhow::Result<Context> {
    let allowed = approved
        .iter()
        .any(|step| tools::data::is_action(&step.call.tool));
    let mut bindings = Vec::new();
    for step in approved {
        for op in &step.ops {
            if let tools::Op::Session {
                command:
                    crate::session::SessionCommand::SignalCapture { uri, auth, .. }
                    | crate::session::SessionCommand::SignalRead { uri, auth, .. }
                    | crate::session::SessionCommand::SignalBrowse { uri, auth, .. },
                ..
            } = op
            {
                let mut binding = tools::context::DataSourceRef::approved(uri)?;
                binding.auth = auth.clone();
                binding.validate_local()?;
                binding.validate_in(&context)?;
                bindings.push(binding);
            }
        }
    }
    agent::authorize_plan_data_with_sources(&mut context, allowed, &bindings)?;
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
    fn test_s3(&self, _: &Checked, _: &dyn SecretSource) -> Result<String, String> {
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
pub(super) fn test_s3(
    services: Arc<dyn Services>,
    checked: Checked,
    secrets: Arc<dyn SecretSource + Send + Sync>,
) -> Result<Receiver<Result<String, String>>, String> {
    spawn("capturefab-assistant-bucket-test", move |sender| {
        let _ = sender.send(services.test_s3(&checked, secrets.as_ref()));
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::tests::{FakeHost, fixture_context, fixture_snapshot};
    use serde_json::json;

    #[test]
    fn native_apply_rebinds_only_reviewed_source_endpoints_before_the_first_effect() {
        let snapshot = fixture_snapshot();
        let binding = tools::context::DataSourceRef::approved("sim-signal://plan-first").unwrap();
        let mut context = fixture_context(snapshot.clone());
        agent::authorize_plan_data_with_sources(&mut context, true, std::slice::from_ref(&binding))
            .unwrap();
        let calls = vec![(
            StepId(1),
            ToolCall {
                tool: "read_data_source".into(),
                args: json!({"source":binding.reference,"timeout":"1s"}),
            },
        )];
        let approved = tools::check_plan(&context, &calls, &|_, _| false);
        assert!(approved[0].ready(), "{:?}", approved[0].issues);
        assert_eq!(approved[0].call.args["source"], binding.reference);
        let host = FakeHost::new(snapshot.clone());
        assert!(host.calls().is_empty());
        tools::apply(
            &host,
            &tools::EmptySecrets,
            || approved_data_context(fixture_context(snapshot.clone()), &approved),
            &calls,
            Some(&approved),
            |_| {},
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(host.calls().iter().any(|(_,command)|matches!(command,crate::session::SessionCommand::SignalRead {uri,..} if uri==&binding.uri)));

        let substituted = FakeHost::new(snapshot.clone());
        let result = tools::apply(
            &substituted,
            &tools::EmptySecrets,
            || {
                let mut context =
                    approved_data_context(fixture_context(snapshot.clone()), &approved)?;
                context.data_sources = vec![tools::context::DataSourceRef::approved(
                    "sim-signal://unreviewed",
                )?];
                Ok(context)
            },
            &calls,
            Some(&approved),
            |_| {},
            &AtomicBool::new(false),
        );
        assert!(result.is_err());
        assert!(
            substituted.calls().is_empty(),
            "substituted bindings fail before any effect"
        );
    }

    #[test]
    fn native_apply_preserves_reviewed_credential_epoch_and_rejects_its_replacement() {
        let mut context = fixture_context(fixture_snapshot());
        let source = crate::gui::source_setup::tests::bound_context(&mut context);
        let fresh = context.clone();
        agent::authorize_plan_data_with_sources(&mut context, true, std::slice::from_ref(&source))
            .unwrap();
        let calls = vec![(
            StepId(1),
            ToolCall {
                tool: "read_data_source".into(),
                args: json!({"source":source.reference,"timeout":"1s"}),
            },
        )];
        let approved = tools::check_plan(&context, &calls, &|_, _| false);
        assert!(approved[0].ready(), "{:?}", approved[0].issues);
        let host = FakeHost::new(context.snapshot.clone());
        tools::apply(
            &host,
            &tools::EmptySecrets,
            || approved_data_context(fresh.clone(), &approved),
            &calls,
            Some(&approved),
            |_| {},
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(host.calls().iter().any(|(_,command)|matches!(command,crate::session::SessionCommand::SignalRead{uri,auth,..} if uri==&source.uri && auth==&source.auth)));
        let mut changed = fresh;
        changed.snapshot.signal_bindings[0].auth.epoch = "e".repeat(32);
        let denied = FakeHost::new(changed.snapshot.clone());
        let result = tools::apply(
            &denied,
            &tools::EmptySecrets,
            || approved_data_context(changed.clone(), &approved),
            &calls,
            Some(&approved),
            |_| {},
            &AtomicBool::new(false),
        );
        assert!(result.is_err());
        assert!(
            denied.calls().is_empty(),
            "epoch replacement fails before the first source effect"
        );
    }
}
