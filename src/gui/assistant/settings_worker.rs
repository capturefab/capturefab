//! Serial blocking work for the assistant Settings section. The UI owns only
//! non-secret settings and short-lived, zeroizing credential drafts.
use super::SecretDraft;
use crate::assistant::{self as ai, CredentialStatus, ProviderId, ProviderSettings, Settings};
use iced::futures::channel::oneshot;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, Sender},
    },
};

pub(in crate::gui) struct Loaded {
    pub settings: Settings,
    pub warnings: Vec<String>,
    pub environment_ready: BTreeSet<ProviderId>,
    pub environment_names: Vec<String>,
    /// Only documented non-secret configuration variables, never API keys.
    pub environment: BTreeMap<String, String>,
}

#[derive(Clone, Debug)]
pub(in crate::gui) struct ConnectionFailure {
    pub message: String,
    pub excerpt: Option<String>,
}

pub(in crate::gui) trait Services: Send + Sync {
    fn load(&self) -> Loaded;
    /// Cheap in-process invalidation only; no filesystem or credential reads.
    fn revision(&self) -> u64 {
        0
    }
    fn save(&self, id: ProviderId, settings: ProviderSettings, select: bool) -> Result<(), String>;
    fn status(&self, id: ProviderId, settings: &ProviderSettings) -> CredentialStatus;
    fn store(&self, id: ProviderId, key: &str) -> Result<(), String>;
    fn remove(&self, id: ProviderId) -> Result<(), String>;
    fn test(&self, id: ProviderId, settings: ProviderSettings) -> Result<(), ConnectionFailure>;
    /// Non-secret configuration only; this runs on the serial worker.
    fn profile_region(&self, _profile: &str) -> Option<String> {
        None
    }
}

pub(in crate::gui) struct Native;
impl Services for Native {
    fn revision(&self) -> u64 {
        ai::settings::revision()
    }
    fn profile_region(&self, profile: &str) -> Option<String> {
        crate::aws::profile_region(profile)
    }
    fn load(&self) -> Loaded {
        let loaded = ai::settings::load();
        let present = |name: &str| std::env::var_os(name).is_some_and(|v| !v.is_empty());
        let environment_ready = ProviderId::ALL
            .into_iter()
            .filter(|id| environment_ready(*id, &present))
            .collect();
        let environment_names = ProviderId::ALL
            .into_iter()
            .flat_map(|id| id.info().key_envs.iter().copied())
            .chain(["GOOGLE_APPLICATION_CREDENTIALS", "AWS_PROFILE"])
            .filter(|name| present(name))
            .map(str::to_owned)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let environment = [
            "CLOUDFLARE_ACCOUNT_ID",
            "AZURE_OPENAI_ENDPOINT",
            "AWS_REGION",
            "AWS_DEFAULT_REGION",
            "AWS_PROFILE",
            "GOOGLE_CLOUD_PROJECT",
            "GOOGLE_CLOUD_LOCATION",
            "OLLAMA_HOST",
        ]
        .into_iter()
        .filter_map(|name| {
            std::env::var(name)
                .ok()
                .filter(|v| !v.is_empty())
                .map(|value| (name.into(), value))
        })
        .collect();
        Loaded {
            settings: loaded.settings,
            warnings: loaded.warnings,
            environment_ready,
            environment_names,
            environment,
        }
    }
    fn save(&self, id: ProviderId, settings: ProviderSettings, select: bool) -> Result<(), String> {
        let result = if select {
            ai::settings::select(id, settings)
        } else {
            ai::settings::set_provider_settings(id, settings)
        };
        result.map_err(|_| {
            "Could not save assistant settings. Check the session folder permissions.".into()
        })
    }
    fn status(&self, id: ProviderId, settings: &ProviderSettings) -> CredentialStatus {
        ai::credentials::status(id, settings, true)
    }
    fn store(&self, id: ProviderId, key: &str) -> Result<(), String> {
        ai::credentials::store_key(id, key.to_owned())
            .map_err(|_| "The OS credential store could not save this key.".into())
    }
    fn remove(&self, id: ProviderId) -> Result<(), String> {
        ai::credentials::delete_key(id)
            .map_err(|_| "The OS credential store could not remove the saved key.".into())
    }
    fn test(&self, id: ProviderId, settings: ProviderSettings) -> Result<(), ConnectionFailure> {
        let deps = ai::agent::Deps::native(
            Arc::new(crate::tools::context::NoSession),
            Settings::default(),
            std::path::PathBuf::new(),
            crate::storage::StoragePolicy::default(),
            crate::tools::context::RecordingDefaults::default(),
        );
        crate::cli::assistant::test_connection(&deps, id, settings, false, &AtomicBool::new(false))
            .map(|_| ())
            .map_err(|error| {
                if let Some(provider) = error.downcast_ref::<ai::ProviderError>() {
                    let excerpt = provider.details()["provider_message"]
                        .as_str()
                        .map(str::to_owned);
                    ConnectionFailure {
                        message: if id == ProviderId::AppleFoundationModels {
                            excerpt.clone().unwrap_or_else(|| provider.to_string())
                        } else {
                            provider.to_string()
                        },
                        excerpt: if id == ProviderId::AppleFoundationModels {
                            None
                        } else {
                            excerpt
                        },
                    }
                } else {
                    ConnectionFailure {
                        message: crate::tools::redact::for_output(serde_json::json!(
                            error.to_string()
                        ))
                        .as_str()
                        .unwrap_or("The connection test failed.")
                        .to_owned(),
                        excerpt: None,
                    }
                }
            })
    }
}

fn environment_ready(id: ProviderId, present: &impl Fn(&str) -> bool) -> bool {
    match id {
        ProviderId::Bedrock => {
            present("AWS_BEARER_TOKEN_BEDROCK")
                || present("AWS_PROFILE")
                || (present("AWS_ACCESS_KEY_ID") && present("AWS_SECRET_ACCESS_KEY"))
        }
        ProviderId::Vertex => {
            present("GOOGLE_APPLICATION_CREDENTIALS")
                || id.info().key_envs.iter().any(|name| present(name))
        }
        _ => id.info().key_envs.iter().any(|name| present(name)),
    }
}

pub(super) enum Work {
    Load(u64, u64),
    Save(ProviderId, ProviderSettings, bool),
    Status(ProviderId, ProviderSettings, u64),
    ProfileRegion(String),
    Store(ProviderId, ProviderSettings, (SecretDraft, u64)),
    Remove(ProviderId, ProviderSettings),
    Test(ProviderId, ProviderSettings, Option<(SecretDraft, u64)>),
    Flush(oneshot::Sender<Result<(), String>>),
}
pub(super) enum Reply {
    Loaded(Loaded, u64, u64),
    Saved(ProviderId, ProviderSettings, Result<(), String>),
    Status(ProviderId, Option<CredentialStatus>),
    ProfileRegion(String, Option<String>),
    Credential(
        ProviderId,
        Option<bool>,
        Result<CredentialStatus, String>,
        Option<(SecretDraft, u64)>,
    ),
    Tested {
        id: ProviderId,
        key_saved: bool,
        status: Option<CredentialStatus>,
        result: Result<(), ConnectionFailure>,
        failed_key: Option<(SecretDraft, u64)>,
    },
}
pub(super) struct Worker {
    pub send: Sender<Work>,
    pub receive: Receiver<Reply>,
}
pub(super) fn start(
    services: Arc<dyn Services>,
    generation: Arc<AtomicU64>,
) -> Result<Worker, String> {
    let (send, work) = mpsc::channel();
    let (reply, receive) = mpsc::channel();
    std::thread::Builder::new()
        .name("capturefab-assistant-settings".into())
        .spawn(move || {
            let mut save_errors = BTreeMap::new();
            while let Ok(work) = work.recv() {
                let response = match work {
                    Work::Load(revision, generation) => {
                        Reply::Loaded(services.load(), revision, generation)
                    }
                    Work::Save(id, settings, select) => {
                        let result = services.save(id, settings.clone(), select);
                        match &result {
                            Ok(()) => {
                                save_errors.remove(&id);
                            }
                            Err(error) => {
                                save_errors.insert(id, error.clone());
                            }
                        }
                        Reply::Saved(id, settings, result)
                    }
                    Work::Status(id, settings, expected) => {
                        let status = (generation.load(Ordering::SeqCst) == expected)
                            .then(|| services.status(id, &settings));
                        Reply::Status(id, status)
                    }
                    Work::ProfileRegion(profile) => {
                        let region = services.profile_region(&profile);
                        Reply::ProfileRegion(profile, region)
                    }
                    Work::Store(id, settings, key) => {
                        let result = services
                            .store(id, key.0.as_str())
                            .map(|_| services.status(id, &settings));
                        let present = result.is_ok().then_some(true);
                        let failed_key = result.is_err().then_some(key);
                        Reply::Credential(id, present, result, failed_key)
                    }
                    Work::Remove(id, settings) => {
                        let result = services.remove(id).map(|_| services.status(id, &settings));
                        Reply::Credential(id, result.is_ok().then_some(false), result, None)
                    }
                    Work::Test(id, settings, key) => {
                        let stored = key
                            .as_ref()
                            .map(|(key, _)| services.store(id, key.as_str()));
                        // A refused store may be retried while its form is still
                        // open. Successful drafts are dropped before a request
                        // can block, and never travel back to the UI.
                        let failed_key = if matches!(&stored, Some(Err(_))) {
                            key
                        } else {
                            drop(key);
                            None
                        };
                        let key_saved = matches!(stored, Some(Ok(())));
                        let status = key_saved.then(|| services.status(id, &settings));
                        let result = match stored {
                            Some(Err(message)) => Err(ConnectionFailure {
                                message,
                                excerpt: None,
                            }),
                            _ => services.test(id, settings),
                        };
                        Reply::Tested {
                            id,
                            key_saved,
                            status,
                            result,
                            failed_key,
                        }
                    }
                    Work::Flush(done) => {
                        let _ = done.send(save_errors.values().next().cloned().map_or(Ok(()), Err));
                        continue;
                    }
                };
                if reply.send(response).is_err() {
                    break;
                }
            }
        })
        .map_err(|_| "Could not start the assistant Settings worker.".to_owned())?;
    Ok(Worker { send, receive })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bedrock_readiness_does_not_treat_a_partial_environment_key_as_ready() {
        for name in [
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_SESSION_TOKEN",
        ] {
            assert!(!environment_ready(ProviderId::Bedrock, &|key| key == name));
        }
        for name in ["AWS_BEARER_TOKEN_BEDROCK", "AWS_PROFILE"] {
            assert!(environment_ready(ProviderId::Bedrock, &|key| key == name));
        }
        assert!(environment_ready(ProviderId::Bedrock, &|key| matches!(
            key,
            "AWS_ACCESS_KEY_ID" | "AWS_SECRET_ACCESS_KEY"
        )));
    }
}
