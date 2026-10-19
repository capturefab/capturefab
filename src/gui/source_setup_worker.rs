//! Serial local profile work. Neither views nor GUI constructors open files,
//! inspect environment values, or access the OS credential store.
use crate::gui::secret::SecretDraft;
use crate::{
    signals::auth::{self, ProfileDraft, ProfileInfo, ProfileRef},
    tools::context::DataSourceRef,
};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
    mpsc::{self, Receiver, Sender},
};

pub(in crate::gui) trait Services: Send + Sync {
    fn list(&self) -> Result<Vec<ProfileInfo>, String>;
    fn edit(&self, profile: &ProfileRef) -> Result<ProfileDraft, String>;
    fn save(
        &self,
        expected: Option<&ProfileRef>,
        draft: ProfileDraft,
        password: Option<SecretDraft>,
    ) -> Result<ProfileInfo, String>;
    fn bind(&self, profile: &ProfileRef) -> Result<DataSourceRef, String>;
    fn delete(&self, profile: &ProfileRef) -> Result<(), String>;
}

pub(in crate::gui) struct Native(pub crate::session::SessionHandle);
fn safe(error: impl std::fmt::Display) -> String {
    crate::tools::redact::for_output_text(&error.to_string())
}
impl Services for Native {
    fn list(&self) -> Result<Vec<ProfileInfo>, String> {
        auth::Store::session().list().map_err(safe)
    }
    fn edit(&self, profile: &ProfileRef) -> Result<ProfileDraft, String> {
        auth::Store::session().load_draft(profile).map_err(safe)
    }
    fn save(
        &self,
        expected: Option<&ProfileRef>,
        draft: ProfileDraft,
        password: Option<SecretDraft>,
    ) -> Result<ProfileInfo, String> {
        let password = password
            .map(|password| auth::Secret::new(password.0))
            .transpose()
            .map_err(safe)?;
        auth::Store::session()
            .save(expected, draft, password)
            .map_err(safe)
    }
    fn bind(&self, profile: &ProfileRef) -> Result<DataSourceRef, String> {
        let value = self
            .0
            .request_with_origin(
                crate::session::SessionCommand::SignalBind {
                    profile: profile.clone(),
                },
                crate::session::ActivityOrigin::Gui,
            )
            .map_err(safe)?;
        let receipt = serde_json::from_value(value)
            .map_err(|_| "The source binding receipt could not be read.".to_string())?;
        DataSourceRef::from_binding(&receipt).map_err(safe)
    }
    fn delete(&self, profile: &ProfileRef) -> Result<(), String> {
        auth::Store::session().delete(profile).map_err(safe)
    }
}
pub(super) struct Unavailable;
impl Services for Unavailable {
    fn list(&self) -> Result<Vec<ProfileInfo>, String> {
        Err("Source profiles are not configured for this session.".into())
    }
    fn edit(&self, _: &ProfileRef) -> Result<ProfileDraft, String> {
        self.list().map(|_| unreachable!())
    }
    fn save(
        &self,
        _: Option<&ProfileRef>,
        _: ProfileDraft,
        _: Option<SecretDraft>,
    ) -> Result<ProfileInfo, String> {
        self.list().map(|_| unreachable!())
    }
    fn bind(&self, _: &ProfileRef) -> Result<DataSourceRef, String> {
        self.list().map(|_| unreachable!())
    }
    fn delete(&self, _: &ProfileRef) -> Result<(), String> {
        self.list().map(|_| ())
    }
}
pub(super) enum Work {
    List,
    Edit(ProfileRef),
    Save {
        expected: Option<ProfileRef>,
        draft: ProfileDraft,
        password: Option<SecretDraft>,
    },
    Bind(ProfileRef),
    Delete(ProfileRef),
}
pub(super) enum Reply {
    List(Result<Vec<ProfileInfo>, String>),
    Edit(ProfileRef, Result<ProfileDraft, String>),
    Saved(Option<ProfileRef>, Result<ProfileInfo, String>),
    Bound(Result<DataSourceRef, String>),
    Deleted(ProfileRef, Result<(), String>),
    Skipped,
}
pub(super) struct Worker {
    pub send: Sender<(u64, Work)>,
    pub receive: Receiver<(u64, Reply)>,
}
pub(super) fn start(
    services: Arc<dyn Services>,
    generation: Arc<AtomicU64>,
) -> Result<Worker, String> {
    let (send, work) = mpsc::channel::<(u64, Work)>();
    let (reply, receive) = mpsc::channel();
    std::thread::Builder::new()
        .name("capturefab-source-profiles".into())
        .spawn(move || {
            while let Ok((expected, work)) = work.recv() {
                let response = if generation.load(Ordering::SeqCst) != expected {
                    Reply::Skipped
                } else {
                    match work {
                        Work::List => Reply::List(services.list()),
                        Work::Edit(profile) => {
                            Reply::Edit(profile.clone(), services.edit(&profile))
                        }
                        Work::Save {
                            expected,
                            draft,
                            password,
                        } => {
                            // A failed credential backend must never reflect the submitted
                            // password into a UI error, even through a third-party error.
                            let redact = password.clone();
                            let result = services.save(expected.as_ref(), draft, password).map_err(
                                |error| match redact
                                    .as_ref()
                                    .map(SecretDraft::as_str)
                                    .filter(|secret| !secret.is_empty())
                                {
                                    Some(secret) => error.replace(secret, "<redacted>"),
                                    None => error,
                                },
                            );
                            drop(redact);
                            Reply::Saved(expected, result)
                        }
                        Work::Bind(profile) => Reply::Bound(services.bind(&profile)),
                        Work::Delete(profile) => {
                            Reply::Deleted(profile.clone(), services.delete(&profile))
                        }
                    }
                };
                if reply.send((expected, response)).is_err() {
                    break;
                }
            }
        })
        .map_err(|_| "Could not start the source profile worker.".to_string())?;
    Ok(Worker { send, receive })
}
