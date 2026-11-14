//! Local exact-worker admission for one industrial source's acquired epoch.
use super::{
    CommandTurn, Coordinator, Held, PendingSlot, Reply, SavedCapture, SessionCommand,
    SessionHandle, Worker, action_key, locked, protocol::WorkerCommand,
};
use crate::types::{Categorized, ErrorKind, Usage};
use anyhow::{Context as _, Result, ensure};
use std::{
    sync::{Arc, PoisonError, Weak, atomic::Ordering, mpsc},
    time::{Duration, Instant},
};

/// An opaque local token: neither a selector nor a PID can recreate this epoch.
#[derive(Clone)]
pub(crate) struct LiveCameraBinding {
    owner: Weak<Coordinator>,
    binding: (String, Arc<Held>),
    worker: Arc<Worker>,
}

const MAX: Duration = Duration::from_secs(30);

/// Convert the original source cutoff conservatively into the shared clock.
pub(super) fn before(deadline: Instant) -> Result<u64> {
    let common = action_key::clock_ns()?;
    let anchor = Instant::now();
    let left = deadline
        .checked_duration_since(anchor)
        .filter(|left| !left.is_zero())
        .ok_or_else(expired)?;
    ensure!(
        left <= MAX,
        Usage("pinned camera deadline exceeds 30s".into())
    );
    common
        .checked_add(u64::try_from(left.as_nanos())?)
        .context("pinned camera deadline overflow")
}

fn expired() -> anyhow::Error {
    Categorized::error(
        ErrorKind::Timeout,
        "pinned camera original operation deadline expired",
    )
}

pub(crate) fn remaining(deadline_ns: u64) -> Result<Duration> {
    let left = deadline_ns
        .checked_sub(action_key::clock_ns()?)
        .filter(|left| *left > 0)
        .ok_or_else(expired)?;
    ensure!(
        left <= 30_000_000_000,
        Usage("pinned camera expiry exceeds 30s".into())
    );
    Ok(Duration::from_nanos(left))
}

pub(crate) fn validate(command: &SessionCommand) -> Result<()> {
    ensure!(
        matches!(
            command,
            SessionCommand::SetValues { dry_run: false, .. }
                | SessionCommand::Schedule { .. }
                | SessionCommand::Forward { .. }
        ),
        Usage(
            "pinned camera admission requires RW setup, a still schedule or video forwarding"
                .into()
        )
    );
    command.validate()
}

impl LiveCameraBinding {
    fn check(&self, handle: &SessionHandle) -> Result<()> {
        ensure!(
            self.owner.ptr_eq(&Arc::downgrade(&handle.inner)),
            Usage("pinned camera belongs to a different coordinator".into())
        );
        if handle.inner.quitting.load(Ordering::Acquire) {
            return Err(super::stopping());
        }
        let (_, held) = handle.command_camera(Some(&self.binding.0), Some(&self.binding))?;
        ensure!(
            held.worker()
                .as_ref()
                .is_some_and(|worker| Arc::ptr_eq(worker, &self.worker)),
            Categorized::error(
                ErrorKind::Disconnected,
                "pinned camera worker changed or exited"
            )
        );
        if let Some(error) = self.worker.shared().retirement() {
            return Err(super::protocol::from_wire(&error));
        }
        Ok(())
    }
}

pub(super) struct Admission {
    pin: LiveCameraBinding,
    deadline_ns: u64,
}

impl Admission {
    pub(super) fn check(&self, handle: &SessionHandle) -> Result<()> {
        remaining(self.deadline_ns)?;
        self.pin.check(handle)?;
        remaining(self.deadline_ns).map(drop)
    }

    fn wait_turn(&self, turn: &CommandTurn, handle: &SessionHandle) -> Result<()> {
        self.check(handle)?;
        let mut state = locked(&turn.order.state);
        while state.pending.front() != Some(&turn.ticket) {
            let wait = remaining(self.deadline_ns)?.min(Duration::from_millis(25));
            if handle.inner.quitting.load(Ordering::Acquire) {
                return Err(super::stopping());
            }
            state = turn
                .order
                .ready
                .wait_timeout(state, wait)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        drop(state);
        self.check(handle)
    }

    pub(super) fn worker(&self, handle: &SessionHandle) -> Result<Arc<Worker>> {
        self.check(handle)?;
        Ok(self.pin.worker.clone())
    }

    pub(super) fn call(
        &self,
        handle: &SessionHandle,
        command: SessionCommand,
    ) -> Result<serde_json::Value> {
        self.check(handle)?;
        let (id, reply) = self.pin.worker.submit(WorkerCommand::SessionUntil {
            command,
            deadline_ns: self.deadline_ns,
        })?;
        loop {
            let left = match remaining(self.deadline_ns) {
                Ok(left) => left,
                Err(error) => {
                    self.pin.worker.cancel(id);
                    // Existing RW execution is not made preemptible: a write
                    // already in progress may have applied before cancellation.
                    return Err(crate::types::with_details(
                        error,
                        serde_json::json!({
                            "camera_operation_dispatched":true,"camera_operation_may_have_applied":true
                        }),
                    ));
                }
            };
            match reply.recv_timeout(left.min(Duration::from_millis(25))) {
                Ok(result) => {
                    let value = result?;
                    self.check(handle)?;
                    return Ok(value);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(self.pin.worker.exit_error().unwrap_or_else(|| {
                        Categorized::error(
                            ErrorKind::Disconnected,
                            "pinned camera worker reply pipe closed",
                        )
                    }));
                }
            }
        }
    }

    pub(super) fn stop_job(&self, job: u64) {
        // Never resolve a replacement worker for uncertain cleanup. A queued
        // expired command cannot subsequently arm; an executing job is stopped.
        let _ = self.pin.worker.stop_job(job);
    }
}

impl SessionHandle {
    /// Pin the already acquired worker, with no lookup discovery or recovery.
    pub(crate) fn pin_existing_camera(&self, selector: &str) -> Result<LiveCameraBinding> {
        let binding = self.target(Some(selector))?;
        if let Some(error) = binding.1.retired_error() {
            return Err(error);
        }
        let worker = binding.1.worker().ok_or_else(|| {
            Categorized::error(
                ErrorKind::Disconnected,
                "pinning requires an existing live camera worker",
            )
        })?;
        let pin = LiveCameraBinding {
            owner: Arc::downgrade(&self.inner),
            binding,
            worker,
        };
        pin.check(self)?;
        Ok(pin)
    }

    pub(crate) fn check_pinned(
        &self,
        binding: &LiveCameraBinding,
        deadline: Instant,
    ) -> Result<()> {
        let admission = Admission {
            pin: binding.clone(),
            deadline_ns: before(deadline)?,
        };
        admission.check(self)
    }

    pub(crate) fn submit_pinned(
        &self,
        binding: &LiveCameraBinding,
        command: SessionCommand,
        deadline: Instant,
    ) -> Result<Reply> {
        validate(&command)?;
        self.submit_bound(binding, command, deadline, None, None)
    }

    pub(crate) fn submit_observed_capture_pinned(
        &self,
        binding: &LiveCameraBinding,
        command: SessionCommand,
        deadline: Instant,
    ) -> Result<(Reply, Arc<SavedCapture>)> {
        ensure!(
            matches!(&command, SessionCommand::Schedule { count: 1, .. }),
            Usage("saved capture observation requires one scheduled still".into())
        );
        validate(&command)?;
        let capture = Arc::new(SavedCapture::default());
        let reply = self.submit_bound(
            binding,
            command,
            deadline,
            Some(Arc::downgrade(&capture)),
            None,
        )?;
        Ok((reply, capture))
    }

    pub(crate) fn submit_action_key_before(
        &self,
        binding: &LiveCameraBinding,
        key: u32,
        deadline: Instant,
    ) -> Result<Reply> {
        self.submit_bound(
            binding,
            SessionCommand::WriteActionDeviceKey { key, timeout_ms: 1 },
            deadline,
            None,
            None,
        )
        .map_err(action_key::no_write)
    }

    pub(super) fn submit_bound(
        &self,
        binding: &LiveCameraBinding,
        command: SessionCommand,
        deadline: Instant,
        capture: Option<Weak<SavedCapture>>,
        forward: Option<Arc<super::ForwardObservation>>,
    ) -> Result<Reply> {
        super::monitor::validate_for_selector(&binding.binding.0, &command)?;
        let admission = Admission {
            pin: binding.clone(),
            deadline_ns: before(deadline)?,
        };
        admission.check(self)?;
        let slot = PendingSlot::acquire(&self.inner, false)?;
        let turn = self
            .command_turn(Some(&binding.binding.0), &command)
            .context("pinned camera command has no ordering gate")?;
        let handle = self.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("capturefab-pinned-command".into())
            .spawn(move || {
                let is_key = matches!(&command, SessionCommand::WriteActionDeviceKey { .. });
                let ordered = admission.wait_turn(&turn, &handle).map_err(|error| {
                    if is_key {
                        action_key::no_write(error)
                    } else {
                        error
                    }
                });
                let result = ordered.and_then(|()| match command {
                    SessionCommand::WriteActionDeviceKey { key, .. } => {
                        action_key::Admission::from_existing(
                            admission.pin.binding.clone(),
                            admission.pin.worker.clone(),
                            key,
                            admission.deadline_ns,
                        )
                        .execute(&handle, None)
                    }
                    command
                    @ (SessionCommand::Schedule { .. } | SessionCommand::Forward { .. }) => handle
                        .start_job(
                            Some(&admission.pin.binding.0),
                            command,
                            capture,
                            Some(&admission.pin.binding),
                            Some(&admission),
                            forward.as_ref(),
                        ),
                    command => {
                        let value = admission.call(&handle, command)?;
                        admission
                            .pin
                            .binding
                            .1
                            .config_unknown
                            .store(false, Ordering::Release);
                        Ok(value)
                    }
                });
                let result = match forward {
                    Some(observation) => observation.admission(result),
                    None => result,
                };
                if let Err(error) = &result {
                    handle.note("error", crate::scrub::redact_urls(&format!("{error:#}")));
                }
                drop(slot);
                let _ = tx.send(result);
            })
            .context("start pinned camera command thread")?;
        Ok(rx)
    }
}

#[cfg(test)]
mod tests;
