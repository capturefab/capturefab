//! Windows stdout helper: one contained process tree and one bounded reader.
use super::{
    Config, quote,
    stream::{self, Buffer, Delivery, End, Mailbox},
};
use crate::{
    timeseries::{Reading, Source},
    types::{Categorized, ErrorKind},
};
use anyhow::Result;
use std::{
    collections::VecDeque,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

mod process;

fn unavailable(message: &'static str) -> anyhow::Error {
    Categorized::error(ErrorKind::Unavailable, message)
}
fn deadline(until: Instant) -> Result<()> {
    if Instant::now() >= until {
        return Err(Categorized::error(
            ErrorKind::Timeout,
            "industrial helper original read deadline expired",
        ));
    }
    Ok(())
}
fn cleanup_timeout() -> anyhow::Error {
    Categorized::new(
        ErrorKind::Timeout,
        "industrial helper reader cleanup was not confirmed before its deadline",
    )
    .details(serde_json::json!({"cleanup":"unconfirmed"}))
    .into()
}
pub(super) fn open(config: &Config) -> Result<Box<dyn Source>> {
    Ok(Box::new(Lazy {
        prepared: Some(quote::prepare(config)?),
        bridge: None,
        cutoff: None,
        failure: None,
        drain_failure: None,
        finished: false,
    }))
}
struct Lazy {
    prepared: Option<quote::Prepared>,
    bridge: Option<Bridge>,
    cutoff: Option<u64>,
    failure: Option<Arc<anyhow::Error>>,
    drain_failure: Option<Arc<anyhow::Error>>,
    finished: bool,
}
#[derive(Debug)]
struct Failed(Arc<anyhow::Error>);
impl std::fmt::Display for Failed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("industrial helper source retired after failure")
    }
}
impl std::error::Error for Failed {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref().as_ref())
    }
}
impl Lazy {
    fn read_until(&mut self, until: Instant, buffered_only: bool) -> Result<Vec<Reading>> {
        if self.bridge.is_none() {
            if Instant::now() >= until {
                return Ok(Vec::new());
            }
            if !stream::startup_admitted(self.cutoff, crate::host::monotonic_ns())? {
                return Ok(Vec::new());
            }
            let prepared = self
                .prepared
                .take()
                .ok_or_else(|| unavailable("industrial helper startup cannot be repeated"))?;
            self.bridge = Some(Bridge::spawn(prepared, self.cutoff, until)?);
        }
        self.bridge
            .as_mut()
            .ok_or_else(|| unavailable("industrial helper did not start"))?
            .read(until, buffered_only)
    }
    fn failed(&mut self, error: anyhow::Error) -> anyhow::Error {
        let cause = Arc::new(error);
        self.failure = Some(cause.clone());
        self.prepared = None;
        if let Some(bridge) = self.bridge.as_ref() {
            bridge.stop();
        }
        Failed(cause).into()
    }
}
impl Source for Lazy {
    fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
        if let Some(error) = &self.failure {
            return Err(Failed(error.clone()).into());
        }
        if self.finished {
            return Err(Categorized::error(
                ErrorKind::OperationFailed,
                "industrial helper source has finished",
            ));
        }
        let result = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| {
                Categorized::error(ErrorKind::Usage, "industrial helper timeout is too large")
            })
            .and_then(|until| self.read_until(until, timeout.is_zero()));
        result.map_err(|error| self.failed(error))
    }
    fn observed_ns(&self) -> Option<u64> {
        self.bridge
            .as_ref()
            .and_then(|bridge| bridge.buffer.observed)
    }
    fn buffered_timestamps(&self) -> bool {
        true
    }
    fn ready(&self) -> bool {
        self.bridge.is_some()
    }
    fn set_acquisition_end(&mut self, end: Option<u64>) {
        if let Some(end) = end {
            self.cutoff = Some(self.cutoff.map_or(end, |old| old.min(end)));
        }
        if let Some(bridge) = self.bridge.as_ref() {
            bridge.mailbox.set_cutoff(self.cutoff);
        }
    }
    fn finish(&mut self, timeout: Duration) -> Result<()> {
        self.finished = true;
        self.prepared = None;
        if let Some(bridge) = self.bridge.as_ref() {
            bridge.stop();
        }
        let until = Instant::now().checked_add(timeout).ok_or_else(|| {
            Categorized::error(
                ErrorKind::Usage,
                "industrial helper cleanup timeout is too large",
            )
        })?;
        let result = self
            .bridge
            .as_mut()
            .map_or(Ok(()), |bridge| bridge.finish(until));
        result.map_err(|error| {
            if self.failure.is_none() {
                let error = Arc::new(error);
                self.failure = Some(error.clone());
                return Failed(error).into();
            }
            error
        })
    }
    fn drain(&mut self) -> Result<Option<Reading>> {
        if let Some(error) = &self.drain_failure {
            return Err(Failed(error.clone()).into());
        }
        let result = self.bridge.as_mut().map_or(Ok(None), Bridge::drain);
        result.map_err(|error| {
            let error = Arc::new(error);
            self.drain_failure = Some(error.clone());
            Failed(error).into()
        })
    }
}
struct Bridge {
    process: process::Process,
    mailbox: Arc<Mailbox>,
    reader: Option<thread::JoinHandle<()>>,
    buffer: Buffer,
    settled: bool,
    deferred: VecDeque<Reading>,
}
impl Bridge {
    fn spawn(prepared: quote::Prepared, cutoff: Option<u64>, until: Instant) -> Result<Self> {
        if !stream::startup_admitted(cutoff, crate::host::monotonic_ns())? {
            return Err(Categorized::error(
                ErrorKind::Timeout,
                "industrial helper acquisition cutoff expired before startup",
            ));
        }
        let (process, reader) = process::Process::spawn(prepared, until)?;
        let mailbox = Arc::new(Mailbox::default());
        mailbox.set_cutoff(cutoff);
        let pump = stream::pump(reader, mailbox.clone(), crate::host::monotonic_ns)
            .map_err(|_| unavailable("could not start industrial helper stdout reader"))?;
        let mut bridge = Self {
            process,
            mailbox,
            reader: Some(pump),
            buffer: Buffer::default(),
            settled: false,
            deferred: VecDeque::new(),
        };
        if !stream::startup_admitted(cutoff, crate::host::monotonic_ns())? {
            let _ = bridge.finish(until);
            return Err(Categorized::error(
                ErrorKind::Timeout,
                "industrial helper acquisition cutoff expired before resume",
            ));
        }
        if let Err(error) = bridge.process.resume(cutoff, until) {
            let _ = bridge.finish(until);
            return Err(error);
        }
        Ok(bridge)
    }
    fn stop(&self) {
        self.mailbox.stop(crate::host::monotonic_ns());
        let _ = self.process.job.stop();
    }
    fn read(&mut self, until: Instant, buffered_only: bool) -> Result<Vec<Reading>> {
        let budget = (!buffered_only).then_some(until);
        let rows = self.buffer.batch(budget)?;
        if !rows.is_empty() {
            return self.publish_rows(rows, budget);
        }
        loop {
            match self.mailbox.take(until) {
                Delivery::Idle => return Ok(Vec::new()),
                Delivery::End(End::Eof) => return Err(self.buffer.eof()),
                Delivery::End(end) => {
                    if let Some(error) = end.error() {
                        return Err(error);
                    }
                    return Ok(Vec::new());
                }
                Delivery::Chunk(chunk) => {
                    self.buffer.append(&chunk)?;
                    let rows = self.buffer.batch(budget)?;
                    if !rows.is_empty() {
                        return self.publish_rows(rows, budget);
                    }
                    if Instant::now() >= until {
                        return Ok(Vec::new());
                    }
                }
            }
        }
    }
    fn publish_rows(
        &mut self,
        rows: Vec<Reading>,
        budget: Option<Instant>,
    ) -> Result<Vec<Reading>> {
        if budget.is_some_and(|until| Instant::now() >= until) {
            self.deferred = rows.into();
            return Err(Categorized::error(
                ErrorKind::Timeout,
                "industrial helper original read deadline expired before publication",
            ));
        }
        Ok(rows)
    }
    fn finish(&mut self, until: Instant) -> Result<()> {
        if self.settled {
            return self.terminal_result();
        }
        self.stop();
        let process = self.process.stop_and_wait(until);
        let reader = loop {
            if self
                .reader
                .as_ref()
                .is_none_or(thread::JoinHandle::is_finished)
            {
                break self.reader.take().map_or(Ok(()), |reader| {
                    reader.join().map_err(|_| {
                        Categorized::error(
                            ErrorKind::OperationFailed,
                            "industrial helper stdout reader failed",
                        )
                    })
                });
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break Err(cleanup_timeout());
            }
            thread::park_timeout(left.min(Duration::from_millis(2)));
        };
        self.settled = process.is_ok() && reader.is_ok();
        process.and(reader).and_then(|()| self.terminal_result())
    }
    fn terminal_result(&self) -> Result<()> {
        match self.mailbox.terminal() {
            Some(End::Eof) => Err(self.buffer.eof()),
            Some(end) => end.error().map_or(Ok(()), Err),
            None => Err(cleanup_timeout()),
        }
    }
    fn drain(&mut self) -> Result<Option<Reading>> {
        if let Some(row) = self.deferred.pop_front() {
            return Ok(Some(row));
        }
        loop {
            if self.buffer.has_line() {
                return self.buffer.drain();
            }
            match self.mailbox.take(Instant::now()) {
                Delivery::Chunk(chunk) => self.buffer.append(&chunk)?,
                Delivery::End(_) | Delivery::Idle => return self.buffer.drain(),
            }
        }
    }
}
impl Drop for Bridge {
    fn drop(&mut self) {
        self.stop();
    }
}
