//! Exact-job startup ownership and real completion, separate from a lost reply.
use super::{
    Coordinator, Reply, SessionCommand, SessionHandle, Worker, locked,
    protocol::{Output, WorkerCommand},
};
use crate::types::{Categorized, ErrorKind, Usage};
use anyhow::{Context as _, Result, ensure};
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{Arc, Condvar, Mutex, PoisonError, Weak, atomic::Ordering, mpsc},
    time::{Duration, Instant},
};

const MAX_METADATA: usize = 64 * 1024;
const WATCH: Duration = Duration::from_millis(20);

#[derive(Debug, Clone)]
pub(crate) struct ForwardOwned {
    pub job: String,
    pub forwarding_id: String,
    pub index_path: String,
    /// Planned video path until the real final outcome confirms its file.
    pub output_path: String,
}

#[derive(Debug)]
struct SharedFailure(Arc<anyhow::Error>);
impl std::fmt::Display for SharedFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("observed video forwarding failed")
    }
}
impl std::error::Error for SharedFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref().as_ref())
    }
}
struct Assigned {
    number: u64,
    worker: Arc<Worker>,
}
#[derive(Default)]
struct State {
    assigned: Option<Assigned>,
    owned: Option<ForwardOwned>,
    end: Option<Value>,
    terminal_seen: bool,
    ownership_error: Option<Arc<anyhow::Error>>,
    end_error: Option<Arc<anyhow::Error>>,
    admission_done: bool,
    admission_error: Option<Arc<anyhow::Error>>,
    refused: bool,
    stop_requested: bool,
    stop_sent: bool,
    request: Option<u64>,
}

/// Opaque receipt retained by a source before its startup reply can be lost.
pub(crate) struct ForwardObservation {
    coordinator: Weak<Coordinator>,
    session: String,
    output: String,
    index: String,
    pub(super) deadline_ns: u64,
    state: Mutex<State>,
    changed: Condvar,
}

pub(super) fn validate(command: &SessionCommand) -> Result<()> {
    let SessionCommand::Forward {
        output,
        on_exists,
        timeline: Some(timeline),
        copy,
        ..
    } = command
    else {
        return Err(Usage("observed forwarding requires local indexed video".into()).into());
    };
    ensure!(
        !output.contains("://")
            && output != "-"
            && !crate::naming::has_placeholder(output)
            && on_exists.is_fail()
            && copy.is_never(),
        Usage(
            "observed forwarding requires literal local output, no replacement and no stream copy"
                .into()
        )
    );
    timeline.validate(output)?;
    command.validate()
}

impl ForwardObservation {
    pub(super) fn new(
        handle: &SessionHandle,
        command: &SessionCommand,
        deadline: Instant,
    ) -> Result<Arc<Self>> {
        validate(command)?;
        let SessionCommand::Forward {
            output,
            timeline: Some(timeline),
            ..
        } = command
        else {
            return Err(Usage("observed forwarding requires indexed video".into()).into());
        };
        let output = std::path::absolute(output)?.to_string_lossy().into_owned();
        let index = std::path::absolute(&timeline.path)?
            .to_string_lossy()
            .into_owned();
        ensure!(
            serde_json::to_vec(&json!([output, index]))?.len() <= MAX_METADATA,
            Usage("observed video paths exceed the metadata bound".into())
        );
        Ok(Arc::new(Self {
            coordinator: Arc::downgrade(&handle.inner),
            session: handle.inner.name.clone(),
            output,
            index,
            deadline_ns: super::pinned::before(deadline)?,
            state: Mutex::default(),
            changed: Condvar::new(),
        }))
    }

    pub(super) fn assign(
        self: &Arc<Self>,
        handle: &SessionHandle,
        number: u64,
        worker: Arc<Worker>,
    ) -> Result<()> {
        ensure!(
            self.coordinator.ptr_eq(&Arc::downgrade(&handle.inner)),
            Usage("forward observation belongs to a different coordinator".into())
        );
        let mut state = locked(&self.state);
        ensure!(
            !state.admission_done,
            Usage("forward observation admission already ended".into())
        );
        ensure!(
            state.assigned.is_none(),
            Usage("forward observation was already assigned".into())
        );
        worker.observe_forward(number, Arc::downgrade(self))?;
        state.assigned = Some(Assigned { number, worker });
        drop(state);
        self.changed.notify_all();
        Ok(())
    }

    pub(super) fn admission(&self, result: Result<Value>) -> Result<Value> {
        let mut state = locked(&self.state);
        state.admission_done = true;
        let result = match result {
            Ok(value) => state
                .ownership_error
                .as_ref()
                .or(state.end_error.as_ref())
                .map_or(Ok(value), |error| Err(SharedFailure(error.clone()).into())),
            Err(error) => {
                let error = Arc::new(error);
                let first = state
                    .admission_error
                    .get_or_insert_with(|| error.clone())
                    .clone();
                Err(SharedFailure(first).into())
            }
        };
        let stop_requested = state.stop_requested;
        drop(state);
        self.changed.notify_all();
        if stop_requested {
            let _ = self.stop();
        }
        result
    }

    fn worker_refused(&self) {
        let mut state = locked(&self.state);
        // Ownership is emitted before a successful installation's reply. A
        // correlated worker refusal without it is definitive, unlike pipe loss.
        if state.owned.is_none() {
            state.refused = true;
        }
        drop(state);
        self.changed.notify_all();
    }

    pub(super) fn reply(&self, ok: bool) {
        let mut state = locked(&self.state);
        if !ok && state.owned.is_none() {
            state.refused = true;
        } else if ok && state.owned.is_none() {
            state.ownership_error.get_or_insert_with(|| {
                Arc::new(incomplete("successful Forward omitted index ownership"))
            });
        }
        drop(state);
        self.changed.notify_all();
    }

    pub(super) fn check_pending(&self, handle: &SessionHandle) -> Result<()> {
        super::pinned::remaining(self.deadline_ns)?;
        ensure!(
            !handle.inner.quitting.load(Ordering::Acquire),
            super::stopping()
        );
        ensure!(
            !locked(&self.state).stop_requested,
            Categorized::error(
                ErrorKind::Interrupted,
                "observed Forward cancelled before dispatch"
            )
        );
        Ok(())
    }

    pub(super) fn wait_turn(
        &self,
        turn: Option<&super::CommandTurn>,
        handle: &SessionHandle,
    ) -> Result<()> {
        self.check_pending(handle)?;
        if let Some(turn) = turn {
            let mut state = locked(&turn.order.state);
            while state.pending.front() != Some(&turn.ticket) {
                self.check_pending(handle)?;
                let wait = super::pinned::remaining(self.deadline_ns)?.min(WATCH);
                state = turn
                    .order
                    .ready
                    .wait_timeout(state, wait)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0;
            }
            drop(state);
        }
        self.check_pending(handle)
    }

    pub(super) fn call(
        self: &Arc<Self>,
        handle: &SessionHandle,
        worker: &Worker,
        command: SessionCommand,
    ) -> Result<Value> {
        super::pinned::remaining(self.deadline_ns)?;
        let stopping = locked(&self.state).stop_requested;
        if stopping {
            self.worker_refused();
            return Err(Categorized::error(
                ErrorKind::Interrupted,
                "observed Forward cancelled before dispatch",
            ));
        }
        ensure!(
            !handle.inner.quitting.load(Ordering::Acquire),
            super::stopping()
        );
        let (id, reply) = worker.submit_forward(
            WorkerCommand::ForwardObservedUntil {
                command,
                deadline_ns: self.deadline_ns,
            },
            Arc::downgrade(self),
        )?;
        let stop = {
            let mut state = locked(&self.state);
            state.request = Some(id);
            state.stop_requested
        };
        if stop {
            worker.cancel(id);
        }
        loop {
            let left = match super::pinned::remaining(self.deadline_ns) {
                Ok(left) => left,
                Err(error) => {
                    worker.cancel(id);
                    return Err(error);
                }
            };
            match reply.recv_timeout(left.min(WATCH)) {
                Ok(Ok(value)) => {
                    super::pinned::remaining(self.deadline_ns)?;
                    return Ok(value);
                }
                // Only the correlated worker Reply marks a definitive refusal.
                // This channel also carries local worker/pipe failures.
                Ok(Err(error)) => return Err(error),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(worker.exit_error().unwrap_or_else(|| {
                        Categorized::error(
                            ErrorKind::WorkerCrashed,
                            "observed Forward reply pipe closed",
                        )
                    }));
                }
            }
        }
    }

    /// Feed only the exact registered worker; no PID or current-Held lookup.
    pub(super) fn event(&self, shared: &super::worker::Shared, output: &Output) {
        let mut state = locked(&self.state);
        let Some(owner) = &state.assigned else {
            return;
        };
        if !std::ptr::eq(owner.worker.shared().as_ref(), shared) {
            return;
        }
        let number = owner.number;
        let result = match output {
            Output::IndexOwned {
                job,
                forwarding_id,
                index_path,
                output_path,
            } if *job == number => (|| -> Result<()> {
                ensure!(
                    state.owned.is_none(),
                    incomplete("duplicate ownership receipt")
                );
                ensure!(
                    forwarding_id.len() == 32
                        && forwarding_id.bytes().all(|byte| byte.is_ascii_hexdigit()),
                    incomplete("invalid forwarding identity")
                );
                ensure!(
                    Path::new(index_path) == Path::new(&self.index)
                        && Path::new(output_path) == Path::new(&self.output),
                    incomplete("ownership paths differ from the admitted job")
                );
                ensure!(
                    serde_json::to_vec(&json!([forwarding_id, index_path, output_path]))?.len()
                        <= MAX_METADATA,
                    incomplete("ownership metadata exceeds its bound")
                );
                state.owned = Some(ForwardOwned {
                    job: format!("{}-{number}", self.session),
                    forwarding_id: forwarding_id.clone(),
                    index_path: index_path.clone(),
                    output_path: output_path.clone(),
                });
                Ok(())
            })(),
            Output::JobEnd {
                job,
                status,
                error,
                summary,
            } if *job == number => (|| -> Result<()> {
                ensure!(
                    !state.terminal_seen,
                    incomplete("duplicate final video outcome")
                );
                // Settlement is independent of whether terminal metadata is valid.
                state.terminal_seen = true;
                ensure!(
                    summary.is_object()
                        && summary["written"].is_u64()
                        && summary["finalized"].is_boolean(),
                    incomplete("invalid final video outcome")
                );
                let mut detail = summary.clone();
                if let Some(error) = error {
                    ensure!(
                        error.is_object()
                            && error["code"].is_string()
                            && error["message"].is_string(),
                        incomplete("invalid final video error detail")
                    );
                    detail["error"].clone_from(error);
                } else {
                    ensure!(
                        !matches!(status, super::protocol::JobStatus::Failed),
                        incomplete("failed video outcome omitted its error detail")
                    );
                }
                ensure!(
                    serde_json::to_vec(&detail)?.len() <= MAX_METADATA,
                    incomplete("final video metadata exceeds its bound")
                );
                if detail["finalized"] == true {
                    ensure!(
                        detail["recording"].is_object()
                            && detail["recording"]["path"].is_string()
                            && detail["recording"]["bytes"].is_u64()
                            && detail["recording"]["truncated"] != true
                            && detail["codec"].is_string()
                            && detail["encoder"].is_string()
                            && detail["accepted"].is_u64()
                            && detail["dropped"].is_u64(),
                        incomplete("finalized video omitted valid recording metadata")
                    );
                }
                if let Some(path) = detail["recording"]["path"].as_str() {
                    ensure!(
                        Path::new(path) == Path::new(&self.output),
                        incomplete("final video path differs from the admitted job")
                    );
                }
                state.end = Some(
                    json!({"job":{"id":format!("{}-{number}",self.session),"state":status},"detail":detail}),
                );
                Ok(())
            })(),
            _ => return,
        };
        if let Err(error) = result {
            let target = if matches!(output, Output::IndexOwned { .. }) {
                &mut state.ownership_error
            } else {
                &mut state.end_error
            };
            target.get_or_insert_with(|| Arc::new(error));
        }
        drop(state);
        self.changed.notify_all();
    }

    pub(crate) fn owned(&self) -> Result<Option<ForwardOwned>> {
        let state = locked(&self.state);
        if let Some(error) = &state.ownership_error {
            return Err(SharedFailure(error.clone()).into());
        }
        Ok(state.owned.clone())
    }

    pub(crate) fn poll_end(&self) -> Result<Option<Value>> {
        let state = locked(&self.state);
        if let Some(error) = &state.ownership_error {
            return Err(SharedFailure(error.clone()).into());
        }
        if let Some(error) = &state.end_error {
            return Err(SharedFailure(error.clone()).into());
        }
        if let Some(end) = &state.end {
            return Ok(Some(end.clone()));
        }
        let worker = state.assigned.as_ref().map(|owner| owner.worker.clone());
        drop(state);
        if let Some(error) = worker.and_then(|worker| worker.exit_error()) {
            return Err(error)
                .context("original video worker exited before finalization confirmation");
        }
        Ok(None)
    }

    pub(super) fn stop(&self) -> Result<()> {
        let mut state = locked(&self.state);
        state.stop_requested = true;
        // Startup's public Book outcome must precede a cleanup-induced end.
        // The source can reach cleanup before its coordinator reply thread.
        if !state.admission_done || state.stop_sent || state.terminal_seen {
            return Ok(());
        }
        let Some(owner) = &state.assigned else {
            return Ok(());
        };
        let (worker, number, request) = (owner.worker.clone(), owner.number, state.request);
        state.stop_sent = true;
        drop(state);
        // Do not block the caller's local settle cutoff behind a pipe write.
        // One stop thread per bounded observation; killing its old worker at
        // the cutoff closes that pipe and lets the thread release its owner.
        std::thread::Builder::new()
            .name("capturefab-video-stop".into())
            .spawn(move || {
                if let Some(request) = request {
                    worker.cancel(request);
                }
                let _ = worker.stop_job(number);
            })
            .context("start exact video stop")?;
        Ok(())
    }

    pub(crate) fn stop_and_wait(&self, timeout: Duration) -> Result<Option<Value>> {
        ensure!(
            !timeout.is_zero() && timeout <= Duration::from_secs(30),
            Usage("video settlement budget must be greater than0 and at most30s".into())
        );
        let deadline = Instant::now()
            .checked_add(timeout)
            .context("video settlement deadline overflow")?;
        let mut first_error = None;
        loop {
            if let Err(error) = self.stop() {
                first_error.get_or_insert(error);
            }
            match self.poll_end() {
                Ok(Some(end)) => return first_error.map_or(Ok(Some(end)), Err),
                Ok(None) => {}
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
            let state = locked(&self.state);
            let settled = state.terminal_seen;
            let refused = state.admission_done
                && (state.assigned.is_none() || state.refused)
                && state.owned.is_none();
            let worker = state.assigned.as_ref().map(|owner| owner.worker.clone());
            drop(state);
            if settled || refused {
                return first_error.map_or(Ok(None), Err);
            }
            if let Some(error) = worker.as_ref().and_then(|worker| worker.exit_error()) {
                return Err(first_error.unwrap_or_else(|| {
                    error.context("original video worker exited before finalization confirmation")
                }));
            }
            let now = Instant::now();
            if now >= deadline {
                if let Some(worker) = worker {
                    worker.kill();
                }
                return Err(first_error.unwrap_or_else(|| {
                    Categorized::error(
                        ErrorKind::Timeout,
                        "original video worker did not settle within its local budget",
                    )
                }));
            }
            drop(
                self.changed
                    .wait_timeout(locked(&self.state), (deadline - now).min(WATCH))
                    .unwrap_or_else(PoisonError::into_inner),
            );
        }
    }
}

fn incomplete(message: &str) -> anyhow::Error {
    Categorized::error(ErrorKind::Incomplete, format!("observed video: {message}"))
}

impl SessionHandle {
    pub(crate) fn submit_observed_forward(
        &self,
        command: SessionCommand,
        deadline: Instant,
    ) -> Result<(Reply, Arc<ForwardObservation>)> {
        let observation = ForwardObservation::new(self, &command, deadline)?;
        let reply =
            self.submit_target_observed(None, command, None, None, Some(observation.clone()))?;
        Ok((reply, observation))
    }
    pub(crate) fn submit_observed_forward_pinned(
        &self,
        binding: &super::LiveCameraBinding,
        command: SessionCommand,
        deadline: Instant,
    ) -> Result<(Reply, Arc<ForwardObservation>)> {
        let observation = ForwardObservation::new(self, &command, deadline)?;
        let reply =
            self.submit_bound(binding, command, deadline, None, Some(observation.clone()))?;
        Ok((reply, observation))
    }
}

#[cfg(test)]
mod tests;
