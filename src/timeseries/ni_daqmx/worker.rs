//! Serial, task-owning private SDK worker. The parent owns process deadlines.
use super::{native::Task, wire};
use crate::types::{Categorized, ErrorKind};
use anyhow::Result;

pub(super) fn run() -> Result<()> {
    // Must precede SDK load: vendor stdout goes to the parent's discarded
    // stderr, while only this File retains the bounded protocol output pipe.
    let mut output = crate::session::protocol_output()?;
    let input = std::io::stdin();
    let mut input = input.lock();
    let mut task = None;
    let mut first_failure = None;
    while let Some(request) = wire::read::<wire::Request>(&mut input)? {
        let finishing = matches!(request, wire::Request::Finish { .. });
        let response = match request {
            wire::Request::Open {
                config,
                deadline_ns,
                cutoff_ns,
            } => {
                if task.is_some() || first_failure.is_some() {
                    failed(
                        first_failure.as_ref(),
                        "NI-DAQmx worker cannot reopen a task",
                    )
                } else {
                    match Task::open(config, deadline_ns, cutoff_ns) {
                        Ok((opened, ready)) => {
                            task = Some(opened);
                            ready
                        }
                        Err(error) => {
                            let error = wire::Failure::from_error(&error);
                            first_failure = Some(error.clone());
                            wire::Response::Failed { error }
                        }
                    }
                }
            }
            wire::Request::Read {
                deadline_ns,
                cutoff_ns,
            } => {
                if first_failure.is_some() {
                    failed(first_failure.as_ref(), "NI-DAQmx worker is retired")
                } else {
                    task.as_mut().map_or_else(
                        || failed(None, "NI-DAQmx worker has no open task"),
                        |task| task.read(deadline_ns, cutoff_ns),
                    )
                }
            }
            wire::Request::Finish { deadline_ns } => task.as_mut().map_or_else(
                || {
                    failed(
                        first_failure.as_ref(),
                        "NI-DAQmx worker has no task to finish",
                    )
                },
                |task| task.finish(deadline_ns),
            ),
        };
        if first_failure.is_none() {
            match &response {
                wire::Response::Read {
                    terminal: Some(error),
                    ..
                }
                | wire::Response::Finished {
                    terminal: Some(error),
                    ..
                }
                | wire::Response::Failed { error } => first_failure = Some(error.clone()),
                _ => {}
            }
        }
        wire::write(&mut output, &response)?;
        if finishing {
            break;
        }
    }
    // Task::drop has no native calls. An uncleared task/library is left for the
    // worker process teardown, never silently reported as SDK cleanup success.
    Ok(())
}

fn failed(first: Option<&wire::Failure>, message: &'static str) -> wire::Response {
    wire::Response::Failed {
        error: first.cloned().unwrap_or_else(|| {
            wire::Failure::from_error(&Categorized::error(ErrorKind::Incomplete, message))
        }),
    }
}
