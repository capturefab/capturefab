//! Exact live-worker admission and cross-process monotonic action-key expiry.
use super::{Cancel, CommandTurn, Held, SessionCommand, SessionHandle, Worker, locked};
use crate::types::{Categorized, ErrorKind, Usage};
use anyhow::{Context as _, Result, ensure};
use serde_json::{Value, json};
use std::{
    sync::{Arc, PoisonError, atomic::Ordering},
    time::{Duration, Instant},
};

const MAX_NS: u64 = 30_000_000_000;

// Rust1.89 Instant uses UPTIME_RAW on Apple, CLOCK_MONOTONIC on Linux and
// QPC on Windows. Keep this operation clock in that same suspend/rate domain.
pub(super) fn clock_ns() -> Result<u64> {
    #[cfg(target_os = "macos")]
    let value = {
        let mut value = std::mem::MaybeUninit::<libc::timespec>::uninit();
        // SAFETY: value points to a complete writable timespec.
        let success = unsafe { libc::clock_gettime(libc::CLOCK_UPTIME_RAW, value.as_mut_ptr()) };
        if success == 0 {
            // SAFETY: successful clock_gettime initialized value.
            let value = unsafe { value.assume_init() };
            u64::try_from(value.tv_sec)
                .ok()
                .and_then(|seconds| seconds.checked_mul(1_000_000_000))
                .zip(u64::try_from(value.tv_nsec).ok())
                .and_then(|(seconds, nanos)| seconds.checked_add(nanos))
        } else {
            None
        }
    };
    #[cfg(any(target_os = "linux", target_os = "android", windows))]
    let value = crate::host::monotonic_ns();
    #[cfg(not(any(
        target_os = "macos",
        target_os = "linux",
        target_os = "android",
        windows
    )))]
    let value: Option<u64> = None;
    value.ok_or_else(|| {
        Categorized::error(
            ErrorKind::Unsupported,
            "camera operation clock is unavailable",
        )
    })
}

fn now_ns() -> Result<u64> {
    clock_ns().map_err(no_write)
}

pub(crate) fn no_write(error: anyhow::Error) -> anyhow::Error {
    crate::types::with_details(
        error,
        json!({"feature":"ActionDeviceKey","write_attempted":false,
        "write_may_have_applied":false,"readback":"unavailable_write_only","rollback":"not_available"}),
    )
}

pub(crate) fn uncertain(error: anyhow::Error) -> anyhow::Error {
    if crate::types::classify(&error)
        .details
        .as_ref()
        .is_some_and(|details| details.get("write_may_have_applied").is_some())
    {
        return error;
    }
    crate::types::with_details(
        error,
        json!({"feature":"ActionDeviceKey","write_may_have_applied":true,
        "readback":"unavailable_write_only","rollback":"not_available"}),
    )
}

/// A local expiry/pipe failure after dispatch cannot say whether the worker
/// reached its write. Discard only our local no-write assertion, retaining the
/// typed cause and every other diagnostic. A received worker error instead
/// goes through `uncertain()`, preserving its explicit preparation result.
pub(crate) fn dispatched(error: &anyhow::Error, completed: bool) -> anyhow::Error {
    let classified = crate::types::classify(error);
    let mut details = classified.details.unwrap_or_else(|| json!({}));
    if !details.is_object() {
        details = json!({"cause":details});
    }
    if let Some(fields) = details.as_object_mut() {
        fields.remove("write_attempted");
        if completed {
            fields.insert("write_attempted".into(), true.into());
        }
        fields.insert("write_may_have_applied".into(), true.into());
        fields.insert("readback".into(), "unavailable_write_only".into());
        fields.insert("rollback".into(), "not_available".into());
    }
    let mut result = Categorized::new(classified.kind, format!("{error:#}")).details(details);
    if let Some(hint) = classified.hint {
        result = result.hint(hint);
    }
    result.into()
}

pub(crate) fn remaining(deadline_ns: u64) -> Result<Duration> {
    let left = deadline_ns
        .checked_sub(now_ns()?)
        .filter(|left| *left > 0)
        .ok_or_else(|| {
            no_write(Categorized::error(
                ErrorKind::Timeout,
                "action key absolute operation deadline expired",
            ))
        })?;
    ensure!(
        left <= MAX_NS,
        no_write(Usage("action key expiry exceeds the admitted 30s bound".into()).into())
    );
    Ok(Duration::from_nanos(left))
}

pub(crate) fn deadline_after(timeout: Duration) -> Result<u64> {
    ensure!(
        !timeout.is_zero() && timeout <= Duration::from_secs(30),
        no_write(Usage("action key timeout must be greater than 0 and at most 30s".into()).into())
    );
    now_ns()?
        .checked_add(u64::try_from(timeout.as_nanos())?)
        .context("action key expiry overflow")
}

pub(crate) fn instant(deadline_ns: u64) -> Result<Instant> {
    // Anchor before reading the common clock. Conversion error can shorten,
    // never extend, the admitted expiry; no fresh duration starts at dequeue.
    let anchor = Instant::now();
    anchor
        .checked_add(remaining(deadline_ns)?)
        .context("action key Instant overflow")
}

pub(super) struct Admission {
    binding: (String, Arc<Held>),
    worker: Arc<Worker>,
    key: u32,
    deadline_ns: u64,
}

impl Admission {
    pub(super) const fn from_existing(
        binding: (String, Arc<Held>),
        worker: Arc<Worker>,
        key: u32,
        deadline_ns: u64,
    ) -> Self {
        Self {
            binding,
            worker,
            key,
            deadline_ns,
        }
    }

    pub(super) fn capture(
        handle: &SessionHandle,
        target: Option<&str>,
        command: &SessionCommand,
    ) -> Result<Option<Self>> {
        let SessionCommand::WriteActionDeviceKey { key, timeout_ms } = command else {
            return Ok(None);
        };
        let deadline_ns = deadline_after(Duration::from_millis(*timeout_ms))?;
        let target = target.ok_or_else(|| {
            no_write(Usage("action key requires an explicit existing camera binding".into()).into())
        })?;
        let binding = handle.target(Some(target)).map_err(no_write)?;
        if let Some(error) = binding.1.retired_error() {
            return Err(no_write(error));
        }
        let worker = binding.1.worker().ok_or_else(|| {
            no_write(Categorized::error(
                ErrorKind::Disconnected,
                "action key requires a currently live camera worker; connect explicitly first",
            ))
        })?;
        remaining(deadline_ns)?;
        Ok(Some(Self {
            binding,
            worker,
            key: *key,
            deadline_ns,
        }))
    }

    fn check(&self, handle: &SessionHandle, cancel: Option<&Cancel>) -> Result<()> {
        if handle.inner.quitting.load(Ordering::Acquire)
            || cancel.is_some_and(|cancel| cancel.cancelled.load(Ordering::Acquire))
        {
            return Err(no_write(Categorized::error(
                ErrorKind::Interrupted,
                "action key cancelled before submission to its worker",
            )));
        }
        remaining(self.deadline_ns).map(drop)
    }

    pub(super) fn wait_turn(
        &self,
        turn: Option<&CommandTurn>,
        handle: &SessionHandle,
        cancel: Option<&Cancel>,
    ) -> Result<()> {
        self.check(handle, cancel)?;
        let Some(turn) = turn else {
            return Ok(());
        };
        let mut state = locked(&turn.order.state);
        while state.pending.front() != Some(&turn.ticket) {
            self.check(handle, cancel)?;
            let wait = remaining(self.deadline_ns)?.min(Duration::from_millis(25));
            state = turn
                .order
                .ready
                .wait_timeout(state, wait)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        self.check(handle, cancel)
    }

    fn current_worker(&self, handle: &SessionHandle) -> Result<()> {
        let (_, held) = handle
            .command_camera(Some(&self.binding.0), Some(&self.binding))
            .map_err(no_write)?;
        ensure!(
            held.worker()
                .as_ref()
                .is_some_and(|worker| Arc::ptr_eq(worker, &self.worker)),
            no_write(Categorized::error(
                ErrorKind::Disconnected,
                "action key camera worker changed or exited before the operation"
            ))
        );
        if let Some(error) = self.worker.shared().retirement() {
            return Err(no_write(super::protocol::from_wire(&error)));
        }
        Ok(())
    }

    pub(super) fn execute(self, handle: &SessionHandle, cancel: Option<&Cancel>) -> Result<Value> {
        self.check(handle, cancel)?;
        self.current_worker(handle)?;
        self.check(handle, cancel)?;
        let command = super::protocol::WorkerCommand::WriteActionDeviceKeyUntil {
            key: self.key,
            deadline_ns: self.deadline_ns,
        };
        let (id, reply) = self.worker.submit(command).map_err(uncertain)?;
        if let Some(cancel) = cancel {
            cancel.running(&self.worker, id);
        }
        loop {
            let left = match remaining(self.deadline_ns) {
                Ok(left) => left,
                Err(error) => {
                    self.worker.cancel(id);
                    // The pipe may already have delivered the write; this is
                    // not proof that a cancellation took it back.
                    return Err(dispatched(&error, false));
                }
            };
            match reply.recv_timeout(left.min(Duration::from_millis(25))) {
                Ok(Ok(value)) => {
                    remaining(self.deadline_ns).map_err(|error| dispatched(&error, true))?;
                    self.current_worker(handle)
                        .map_err(|error| dispatched(&error, true))?;
                    remaining(self.deadline_ns).map_err(|error| dispatched(&error, true))?;
                    return Ok(value);
                }
                Ok(Err(error)) => return Err(uncertain(error)),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(uncertain(self.worker.exit_error().unwrap_or_else(|| {
                        Categorized::error(
                            ErrorKind::Disconnected,
                            "action key worker reply pipe closed",
                        )
                    })));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
