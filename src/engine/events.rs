//! What a camera worker tells its coordinator: replies, saved files, job
//! progress and ends, and log lines, in the order they happened. The camera
//! thread hands lines to an outbox and never waits for the writer that
//! empties it: a job's newest progress replaces its unsent older one, and
//! log lines are dropped once the backlog is long; replies, saved files and
//! job ends are always kept.
use crate::session::protocol::Output;
use std::{
    collections::VecDeque,
    sync::{Arc, Condvar, Mutex, PoisonError},
    time::Duration,
};

/// Lines waiting past which log lines are dropped.
const BACKLOG: usize = 4096;
/// Lines an outbox without a writer keeps, for tests that read them.
const DETACHED: usize = 10_000;

#[derive(Clone)]
pub(crate) struct Events(Arc<Inner>);

struct Inner {
    outbox: Mutex<Outbox>,
    ready: Condvar,
    /// Info lines are sent too, not only warnings and errors.
    verbose: bool,
    /// A writer empties this outbox; without one, the oldest lines go.
    attached: bool,
}

#[derive(Default)]
struct Outbox {
    lines: VecDeque<Output>,
    closed: bool,
}

impl Events {
    /// An outbox a worker's writer empties.
    pub(crate) fn attached(verbose: bool) -> Self {
        Self::new(verbose, true)
    }
    /// An outbox nothing empties, for a camera thread run in this process.
    pub(crate) fn detached() -> Self {
        Self::new(true, false)
    }
    fn new(verbose: bool, attached: bool) -> Self {
        Self(Arc::new(Inner {
            outbox: Mutex::default(),
            ready: Condvar::new(),
            verbose,
            attached,
        }))
    }
    fn outbox(&self) -> std::sync::MutexGuard<'_, Outbox> {
        self.0.outbox.lock().unwrap_or_else(PoisonError::into_inner)
    }
    pub(crate) fn emit(&self, output: Output) {
        // What was counted is on disk before the client hears that its
        // request or job ended, in case it ends this process then.
        if matches!(output, Output::Reply { .. } | Output::JobEnd { .. }) {
            crate::copies::report();
        }
        let mut outbox = self.outbox();
        if outbox.closed {
            return;
        }
        if !self.0.attached && outbox.lines.len() >= DETACHED {
            outbox.lines.pop_front();
        }
        outbox.lines.push_back(output);
        drop(outbox);
        self.0.ready.notify_one();
    }
    /// Job `job`'s state now, replacing a progress line of it still waiting.
    pub(crate) fn progress(&self, job: u64, progress: serde_json::Value) {
        let mut outbox = self.outbox();
        if let Some(Output::Progress {
            progress: waiting, ..
        }) = outbox
            .lines
            .iter_mut()
            .rev()
            .find(|line| matches!(line, Output::Progress { job: other, .. } if *other == job))
        {
            *waiting = progress;
            return;
        }
        drop(outbox);
        self.emit(Output::Progress { job, progress });
    }
    /// A log line: warnings and errors, and info lines when verbose. Lines
    /// past a long backlog are dropped rather than held.
    pub(crate) fn log(&self, level: &str, message: &str) {
        if level == "info" && !self.0.verbose {
            return;
        }
        if self.0.attached && self.outbox().lines.len() >= BACKLOG {
            return;
        }
        self.emit(Output::Log {
            level: level.into(),
            message: message.into(),
        });
    }
    /// The next line for the writer, waiting up to `wait`; None once closed
    /// and empty, or when nothing came.
    pub(crate) fn next(&self, wait: Duration) -> Next {
        let mut outbox = self.outbox();
        loop {
            if let Some(line) = outbox.lines.pop_front() {
                return Next::Line(Box::new(line));
            }
            if outbox.closed {
                return Next::Closed;
            }
            let (next, timeout) = self
                .0
                .ready
                .wait_timeout(outbox, wait)
                .unwrap_or_else(PoisonError::into_inner);
            outbox = next;
            if timeout.timed_out() && outbox.lines.is_empty() {
                return Next::Idle;
            }
        }
    }
    /// No more lines are taken; the writer sends what is waiting and ends.
    pub(crate) fn close(&self) {
        self.outbox().closed = true;
        self.0.ready.notify_all();
    }
    /// Every line waiting, for tests of a camera thread run in this process.
    #[cfg(test)]
    pub(crate) fn drain(&self) -> Vec<Output> {
        self.outbox().lines.drain(..).collect()
    }
}

pub(crate) enum Next {
    Line(Box<Output>),
    Idle,
    Closed,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn progress_is_coalesced_and_other_lines_keep_their_order() {
        let events = Events::attached(false);
        events.progress(1, json!({"saved": 1}));
        events.emit(Output::Accepted { id: 9 });
        events.progress(1, json!({"saved": 2}));
        events.progress(2, json!({"saved": 1}));
        events.log("info", "quiet");
        events.log("warn", "loud");
        let lines = (0..4)
            .map(|_| match events.next(Duration::ZERO) {
                Next::Line(line) => serde_json::to_value(line).unwrap(),
                _ => panic!("missing line"),
            })
            .collect::<Vec<_>>();
        assert_eq!(lines[0]["progress"], json!({"saved": 2}));
        assert_eq!(lines[1]["event"], "accepted");
        assert_eq!(lines[2]["job"], 2);
        assert_eq!(lines[3]["message"], "loud");
        assert!(matches!(events.next(Duration::ZERO), Next::Idle));
        events.close();
        assert!(matches!(events.next(Duration::ZERO), Next::Closed));
    }

    #[test]
    fn a_long_backlog_drops_log_lines_but_never_job_ends() {
        let events = Events::attached(true);
        for _ in 0..BACKLOG + 10 {
            events.log("warn", "noise");
        }
        events.emit(Output::JobEnd {
            job: 1,
            status: crate::session::protocol::JobStatus::Done,
            error: None,
            summary: json!({}),
        });
        let mut count = 0;
        let mut ended = false;
        while let Next::Line(line) = events.next(Duration::ZERO) {
            count += 1;
            ended |= matches!(*line, Output::JobEnd { .. });
        }
        assert_eq!(count, BACKLOG + 1);
        assert!(ended);
    }
}
