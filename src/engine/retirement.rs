//! Terminal transport loss ends one binding, while owned frames keep saving.
use super::{
    Camera, CaptureStatus, CommandState, JobStatus, JobStream, Output, SessionCommand,
    WorkerCommand, WorkerHandle, forward_failed, inspector, jobs_changed,
};
use crate::session::protocol;
use serde_json::Value;
use std::sync::PoisonError;

pub(crate) fn wire_is_retired(error: &Value) -> bool {
    let classified = crate::types::classify(&protocol::from_wire(error));
    classified.kind == crate::types::ErrorKind::Disconnected
        && classified
            .details
            .as_ref()
            .is_some_and(|details| details["controller_ownership"] == "retired")
        || monitor_retired(&protocol::from_wire(error))
}

fn monitor_retired(error: &anyhow::Error) -> bool {
    crate::types::classify(error)
        .details
        .as_ref()
        .is_some_and(|details| details["monitor_connection"]["state"] == "retired")
}

fn terminal_retired(error: &anyhow::Error, monitor: bool) -> bool {
    crate::transport::gige::ownership_retired(error) || (monitor && monitor_retired(error))
}

pub(super) fn admission(h: &WorkerHandle, command: &WorkerCommand) -> Option<anyhow::Error> {
    if matches!(
        command.session(),
        Some(SessionCommand::Disconnect | SessionCommand::Jobs | SessionCommand::Status)
    ) {
        return None;
    }
    h.retired
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .map(protocol::from_wire)
}

/// Check sticky backend state as well as returned errors: auto/tick/cache paths
/// may log or consume their failure, and an idle binding still needs retiring.
pub(super) fn check(
    h: &WorkerHandle,
    state: CommandState<'_>,
    inspector: &mut inspector::Inspector,
    observed: Option<&anyhow::Error>,
) -> bool {
    if h.retired
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .is_some()
    {
        return false;
    }
    let monitor = state
        .camera
        .as_ref()
        .is_some_and(Camera::is_read_only_monitor);
    let error = state
        .camera
        .as_ref()
        .and_then(Camera::terminal_error)
        .filter(|error| terminal_retired(error, monitor));
    let observed = observed.filter(|error| terminal_retired(error, monitor));
    let wire = error.as_ref().or(observed).map(protocol::wire_error);
    let Some(mut wire) = wire.filter(wire_is_retired) else {
        return false;
    };
    let CommandState {
        camera,
        forwarder,
        auto,
        jobs,
        scheduled_stream,
        ..
    } = state;
    if let Some(camera) = camera.as_ref() {
        wire["hint"] = serde_json::json!(format!(
            "connect a fresh camera binding with capturefab session start --name {} -c {}",
            crate::hint::shell_quote(&h.session),
            crate::hint::shell_quote(&crate::media::redact_url(&camera.info.id)),
        ));
    }
    *h.retired.lock().unwrap_or_else(PoisonError::into_inner) = Some(wire.clone());
    h.events.emit(Output::Retired {
        error: wire.clone(),
    });
    // Retain final local counters before dropping the camera/transport. Later
    // refresh(None) keeps these for saver results and recording finalization.
    if let Some(camera) = camera.as_ref() {
        h.state
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .transport = camera.stats();
    }
    let error = protocol::from_wire(&wire);
    for job in jobs.iter_mut().filter(|job| job.wants()) {
        job.tally(h);
        job.info.end(CaptureStatus::Failed, Some(&error));
        job.deadline = None;
        job.warm = None;
        job.trigger = None;
        job.next = None;
    }
    // Only requests still waiting for frames fail; inspector-owned frames and
    // saver/encoder jobs already holding immutable buffers remain untouched.
    inspector.expire(h, camera, Some(&|| protocol::from_wire(&wire)));
    if forwarder.is_some() {
        forward_failed(h, forwarder, auto, &error);
    }
    drop(auto.take()); // No remote revert/cleanup on a retired connection.
    let auto_job = h
        .auto_job
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
    if let Some(job) = auto_job {
        h.events.emit(Output::JobEnd {
            job,
            status: JobStatus::Failed,
            error: Some(wire.clone()),
            summary: serde_json::json!({}),
        });
    }
    h.state.write().unwrap_or_else(PoisonError::into_inner).auto = None;
    *scheduled_stream = JobStream::default();
    // Camera/transport Drop must perform only local cleanup once retired;
    // central GVCP gating is responsible for prohibiting stale remote writes.
    drop(camera.take());
    h.lock.lock().unwrap_or_else(PoisonError::into_inner).take();
    h.counters.set_fps(0.0);
    h.log("error", format!("Camera binding retired: {error:#}"));
    jobs_changed(h, None, jobs);
    true
}

#[cfg(test)]
mod tests;
