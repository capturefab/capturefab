//! One isolated worker per camera, shared by a run's still and video sources.
use crate::{
    controls::Assignment,
    session::{
        ForwardObservation, LiveCameraBinding, Reply, SavedCapture, SessionCommand, SessionHandle,
    },
    types::{Categorized, ErrorKind, Usage},
};
use anyhow::Result;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex, TryLockError,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Default)]
pub(super) struct Cameras {
    sessions: BTreeMap<String, Arc<Shared>>,
}
pub(super) struct Shared {
    pub(super) handle: SessionHandle,
    selector: String,
    setup: Mutex<Setup>,
    keyed: AtomicBool,
}
struct Setup {
    initialized: bool,
    settings: Vec<Assignment>,
    action_device_key: Option<u32>,
    key_state: KeyState,
}

enum KeyState {
    Pending,
    Ready(LiveCameraBinding),
    Failed(Arc<anyhow::Error>),
}

#[derive(Debug)]
struct SharedFailure(Arc<anyhow::Error>);
impl std::fmt::Display for SharedFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("industrial camera key initialization or admission failed")
    }
}
impl std::error::Error for SharedFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref().as_ref())
    }
}

fn declaration(adapter: &super::Adapter) -> Option<(&str, Option<u32>, &[String])> {
    match adapter {
        super::Adapter::Camera(config) => {
            Some((&config.camera, config.action_device_key, &config.settings))
        }
        super::Adapter::Video(config) => {
            Some((&config.camera, config.action_device_key, &config.settings))
        }
        _ => None,
    }
}

/// Reject new declaration conflicts before any adapter opens or output is made.
pub(super) fn validate_keys(sources: &[super::SourceConfig]) -> Result<()> {
    let mut keys = BTreeMap::new();
    for source in sources {
        if let Some((selector, key, settings)) = declaration(&source.adapter)
            && crate::session::monitor::is_selector(selector)?
        {
            let warmup = match &source.adapter {
                super::Adapter::Camera(config) => config.warmup_ms,
                super::Adapter::Video(config) => config.warmup_ms,
                _ => 0,
            };
            if key.is_some() || !settings.is_empty() || warmup > 0 {
                return Err(crate::session::monitor::unsupported());
            }
        }
        if let Some((selector, Some(key), _)) = declaration(&source.adapter)
            && keys.insert(selector, key).is_some_and(|old| old != key)
        {
            return Err(Usage(
                "shared camera sources have conflicting action_device_key declarations".into(),
            )
            .into());
        }
    }
    if keys.is_empty() {
        return Ok(());
    }
    for source in sources {
        if let Some((selector, _, settings)) = declaration(&source.adapter)
            && keys.contains_key(selector)
        {
            for setting in settings {
                if Assignment::parse(setting)?.name == "ActionDeviceKey" {
                    return Err(Usage("action_device_key cannot be combined with an ActionDeviceKey RW setting on the shared camera".into()).into());
                }
            }
        }
    }
    Ok(())
}

impl Setup {
    fn merge_key(&mut self, key: Option<u32>, settings: &[Assignment]) -> Result<()> {
        if self.action_device_key.is_some() && !matches!(self.key_state, KeyState::Pending)
            || key.is_some() && self.initialized
        {
            return Err(
                Usage("camera key declarations must finish before initialization".into()).into(),
            );
        }
        if self
            .action_device_key
            .zip(key)
            .is_some_and(|(old, new)| old != new)
        {
            return Err(Usage(
                "shared camera sources have conflicting action_device_key declarations".into(),
            )
            .into());
        }
        if self.action_device_key.or(key).is_some()
            && self
                .settings
                .iter()
                .chain(settings)
                .any(|setting| setting.name == "ActionDeviceKey")
        {
            return Err(Usage("action_device_key cannot be combined with an ActionDeviceKey RW setting on the shared camera".into()).into());
        }
        self.action_device_key = self.action_device_key.or(key);
        Ok(())
    }

    fn fail(&mut self, error: anyhow::Error) -> anyhow::Error {
        let cause = match &self.key_state {
            KeyState::Failed(error) => error.clone(),
            KeyState::Pending | KeyState::Ready(_) => {
                let error = Arc::new(error);
                self.key_state = KeyState::Failed(error.clone());
                error
            }
        };
        SharedFailure(cause).into()
    }
}

impl Cameras {
    pub(super) fn get(
        &mut self,
        selector: &str,
        settings: &[Assignment],
        key: Option<u32>,
    ) -> Result<Arc<Shared>> {
        let normalized;
        let selector = if crate::session::monitor::is_selector(selector)? {
            if !settings.is_empty() || key.is_some() {
                return Err(crate::session::monitor::unsupported());
            }
            normalized = crate::transport::gige::monitor::MonitorSpec::parse(selector)?.canonical();
            normalized.as_str()
        } else {
            selector
        };
        let shared = self.sessions.entry(selector.into()).or_insert_with(|| {
            let handle = SessionHandle::new();
            crate::cli::on_second_signal(handle.killer());
            Arc::new(Shared {
                handle,
                selector: selector.into(),
                setup: Mutex::new(Setup {
                    initialized: false,
                    settings: Vec::new(),
                    action_device_key: None,
                    key_state: KeyState::Pending,
                }),
                keyed: AtomicBool::new(false),
            })
        });
        let mut setup = shared
            .setup
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        setup.merge_key(key, settings)?;
        shared
            .keyed
            .store(setup.action_device_key.is_some(), Ordering::Release);
        for setting in settings {
            if let Some(previous) = setup
                .settings
                .iter()
                .find(|previous| previous.target() == setting.target())
            {
                if previous != setting {
                    return Err(Usage(format!(
                        "camera {} has conflicting settings across sources",
                        setting.name
                    ))
                    .into());
                }
            } else {
                setup.settings.push(setting.clone());
            }
        }
        if setup.settings.len() > crate::session::MAX_SET_VALUES {
            return Err(Usage("shared camera settings exceeds 32 assignments".into()).into());
        }
        drop(setup);
        Ok(shared.clone())
    }
}
fn remaining(deadline: Instant) -> Result<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(Categorized::error(
            ErrorKind::Timeout,
            "camera source operation exceeded its deadline",
        ));
    }
    Ok(left)
}
impl Shared {
    pub(super) fn initialize(&self, deadline: Instant) -> Result<()> {
        let mut setup = loop {
            match self.setup.try_lock() {
                Ok(setup) => break setup,
                Err(TryLockError::Poisoned(error)) => break error.into_inner(),
                Err(TryLockError::WouldBlock) => {
                    std::thread::sleep(remaining(deadline)?.min(Duration::from_millis(5)));
                }
            }
        };
        if setup.action_device_key.is_some() {
            return self.initialize_keyed(&mut setup, deadline);
        }
        if setup.initialized {
            return Ok(());
        }
        let timeout_ms = u64::try_from(remaining(deadline)?.as_millis())
            .unwrap_or(u64::MAX)
            .max(1);
        self.call(
            SessionCommand::connect(self.selector.clone(), timeout_ms),
            deadline,
        )?;
        if !setup.settings.is_empty() {
            self.call(
                SessionCommand::SetValues {
                    values: setup.settings.clone(),
                    dry_run: false,
                },
                deadline,
            )?;
        }
        setup.initialized = true;
        Ok(())
    }

    fn initialize_keyed(&self, setup: &mut Setup, deadline: Instant) -> Result<()> {
        match &setup.key_state {
            KeyState::Failed(error) => return Err(SharedFailure(error.clone()).into()),
            KeyState::Ready(binding) => {
                return self
                    .handle
                    .check_pinned(binding, deadline)
                    .map_err(|error| setup.fail(error));
            }
            KeyState::Pending => {}
        }
        let key = setup.action_device_key.ok_or_else(|| {
            Categorized::error(
                ErrorKind::OperationFailed,
                "camera key declaration is missing",
            )
        })?;
        let result = (|| {
            let timeout_ms = u64::try_from(remaining(deadline)?.as_millis())
                .unwrap_or(u64::MAX)
                .max(1);
            self.wait_reply(
                &self
                    .handle
                    .submit(SessionCommand::connect(self.selector.clone(), timeout_ms))?,
                deadline,
            )?;
            remaining(deadline)?;
            self.handle.pin_existing_camera(&self.selector)
        })()
        .map_err(|error| preparation(error, "connect"));
        let binding = match result {
            Ok(binding) => binding,
            Err(error) => return Err(setup.fail(error)),
        };
        if !setup.settings.is_empty() {
            let result = self
                .handle
                .submit_pinned(
                    &binding,
                    SessionCommand::SetValues {
                        values: setup.settings.clone(),
                        dry_run: false,
                    },
                    deadline,
                )
                .and_then(|reply| self.wait_reply(&reply, deadline))
                .and_then(|_| self.handle.check_pinned(&binding, deadline))
                .map_err(|error| preparation(error, "rw_setup"));
            if let Err(error) = result {
                return Err(setup.fail(error));
            }
        }
        let result = self
            .handle
            .submit_action_key_before(&binding, key, deadline)
            .and_then(|reply| self.wait_reply(&reply, deadline))
            .map_err(|error| {
                crate::types::with_details(
                    crate::session::action_key::uncertain(error),
                    json!({"initialization_stage":"write_key"}),
                )
            });
        if let Err(error) = result {
            return Err(setup.fail(error));
        }
        if let Err(error) = self.handle.check_pinned(&binding, deadline) {
            return Err(setup.fail(crate::types::with_details(
                crate::session::action_key::dispatched(&error, true),
                json!({"initialization_stage":"publish_ready"}),
            )));
        }
        setup.key_state = KeyState::Ready(binding);
        Ok(())
    }

    fn ready_binding(&self, deadline: Instant) -> Result<Option<LiveCameraBinding>> {
        let mut setup = loop {
            match self.setup.try_lock() {
                Ok(setup) => break setup,
                Err(TryLockError::Poisoned(error)) => break error.into_inner(),
                Err(TryLockError::WouldBlock) => {
                    std::thread::sleep(remaining(deadline)?.min(Duration::from_millis(5)));
                }
            }
        };
        if setup.action_device_key.is_none() {
            return Ok(None);
        }
        match &setup.key_state {
            KeyState::Ready(binding) => Ok(Some(binding.clone())),
            KeyState::Failed(error) => Err(SharedFailure(error.clone()).into()),
            KeyState::Pending => Err(setup.fail(preparation(
                Categorized::error(ErrorKind::OperationFailed, "camera key has not initialized"),
                "admission",
            ))),
        }
    }

    fn latch(&self, error: anyhow::Error) -> anyhow::Error {
        self.setup
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .fail(error)
    }

    pub(super) fn call(&self, command: SessionCommand, deadline: Instant) -> Result<Value> {
        // These operate on the already-owned job book, including after loss.
        if self.keyed.load(Ordering::Acquire)
            && !matches!(
                command,
                SessionCommand::Wait { .. } | SessionCommand::StopJob { .. }
            )
            && let Some(binding) = self.ready_binding(deadline)?
        {
            return self
                .handle
                .submit_pinned(&binding, command, deadline)
                .and_then(|reply| self.wait_reply(&reply, deadline))
                .and_then(|value| {
                    self.handle.check_pinned(&binding, deadline)?;
                    Ok(value)
                })
                .map_err(|error| self.latch(error));
        }
        remaining(deadline)?;
        self.wait_reply(&self.handle.submit(command)?, deadline)
    }
    pub(super) fn capture(
        &self,
        command: SessionCommand,
        deadline: Instant,
        pending: &mut Option<Arc<SavedCapture>>,
    ) -> Result<(Value, Arc<SavedCapture>)> {
        if self.keyed.load(Ordering::Acquire)
            && let Some(binding) = self.ready_binding(deadline)?
        {
            let result = self
                .handle
                .submit_observed_capture_pinned(&binding, command, deadline)
                .and_then(|(reply, saved)| {
                    *pending = Some(saved.clone());
                    let value = self.wait_reply(&reply, deadline)?;
                    self.handle.check_pinned(&binding, deadline)?;
                    Ok((value, saved))
                });
            return result.map_err(|error| self.latch(error));
        }
        remaining(deadline)?;
        let (reply, saved) = self.handle.submit_observed_capture(command)?;
        Ok((self.wait_reply(&reply, deadline)?, saved))
    }
    /// Keep the exact old recording observation before waiting for startup.
    /// Its caller owns bounded stop/settle even when this reply is lost.
    pub(super) fn forward(
        &self,
        command: SessionCommand,
        deadline: Instant,
        pending: &mut Option<Arc<ForwardObservation>>,
    ) -> Result<Value> {
        if self.keyed.load(Ordering::Acquire)
            && let Some(binding) = self.ready_binding(deadline)?
        {
            let result = self
                .handle
                .submit_observed_forward_pinned(&binding, command, deadline)
                .and_then(|(reply, observed)| {
                    *pending = Some(observed);
                    let value = Self::wait_forward_reply(&reply, deadline)?;
                    self.handle.check_pinned(&binding, deadline)?;
                    Ok(value)
                });
            return result.map_err(|error| self.latch(error));
        }
        remaining(deadline)?;
        let (reply, observed) = self.handle.submit_observed_forward(command, deadline)?;
        *pending = Some(observed);
        Self::wait_forward_reply(&reply, deadline)
    }

    fn wait_forward_reply(reply: &Reply, deadline: Instant) -> Result<Value> {
        // A timed-out Forward may already own an encoder/index. Do not use
        // the generic killer: Video::finish settles only that observed job.
        match reply.recv_timeout(remaining(deadline)?) {
            Ok(result) => {
                let value = result?;
                remaining(deadline)?;
                Ok(value)
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(Categorized::error(
                ErrorKind::Timeout,
                "camera source operation exceeded its deadline",
            )),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(Categorized::error(
                ErrorKind::WorkerCrashed,
                "camera coordinator closed its reply",
            )),
        }
    }
    fn wait_reply(&self, reply: &Reply, deadline: Instant) -> Result<Value> {
        let left = match remaining(deadline) {
            Ok(left) => left,
            Err(error) => {
                self.handle.killer()();
                return Err(error);
            }
        };
        match reply.recv_timeout(left) {
            Ok(result) => result,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                self.handle.killer()();
                Err(Categorized::error(
                    ErrorKind::Timeout,
                    "camera source operation exceeded its deadline",
                ))
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(Categorized::error(
                ErrorKind::WorkerCrashed,
                "camera coordinator closed its reply",
            )),
        }
    }
}

fn preparation(error: anyhow::Error, stage: &str) -> anyhow::Error {
    crate::types::with_details(
        crate::session::action_key::no_write(error),
        json!({"initialization_stage":stage}),
    )
}

#[cfg(test)]
mod tests;
impl Drop for Shared {
    fn drop(&mut self) {
        self.handle.shutdown();
    }
}
