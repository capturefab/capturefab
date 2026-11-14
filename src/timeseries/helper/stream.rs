//! Platform-neutral bounded JSONL buffering and one reserved reader slot.
use crate::{
    timeseries::Reading,
    types::{Categorized, ErrorKind},
};
use anyhow::Result;
use std::{
    io::{self, Read, Write},
    sync::{Arc, Condvar, Mutex},
    thread,
    time::Instant,
};

pub(super) const CHUNK: usize = 8192;
const MAX_LINE: usize = 1 << 20;
const MAX_BATCH: usize = 256;
fn failure(kind: ErrorKind, message: &'static str) -> anyhow::Error {
    Categorized::error(kind, message)
}
pub(super) fn startup_admitted(cutoff: Option<u64>, now: Option<u64>) -> Result<bool> {
    let now = now.ok_or_else(|| {
        failure(
            ErrorKind::Unavailable,
            "industrial helper receive clock is unavailable",
        )
    })?;
    Ok(cutoff.is_none_or(|end| now < end))
}
fn expired() -> anyhow::Error {
    failure(
        ErrorKind::Timeout,
        "industrial helper original parsing deadline expired",
    )
}
fn time_check(deadline: Option<Instant>) -> Result<()> {
    if deadline.is_some_and(|until| Instant::now() >= until) {
        return Err(expired());
    }
    Ok(())
}
#[derive(Default)]
struct Size {
    bytes: usize,
    deadline: Option<Instant>,
}
impl Write for Size {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.deadline.is_some_and(|until| Instant::now() >= until) {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "industrial helper serialization deadline",
            ));
        }
        self.bytes = self.bytes.saturating_add(bytes.len());
        if self.bytes > MAX_LINE - 2 {
            return Err(io::Error::other("industrial helper batch budget"));
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
#[derive(Default)]
pub(super) struct Buffer {
    pending: Vec<u8>,
    line: u64,
    pub(super) observed: Option<u64>,
}
impl Buffer {
    pub(super) fn has_line(&self) -> bool {
        self.pending.contains(&b'\n')
    }
    pub(super) fn append(&mut self, chunk: &Chunk) -> Result<()> {
        if chunk.bytes.len() > CHUNK
            || self.pending.len().saturating_add(chunk.bytes.len()) > MAX_LINE + CHUNK
        {
            return Err(failure(
                ErrorKind::OperationFailed,
                "industrial helper JSON line exceeds 1 MiB",
            ));
        }
        self.pending.extend_from_slice(&chunk.bytes);
        self.observed = Some(chunk.observed);
        Ok(())
    }
    fn reading(bytes: &[u8], line: u64) -> Result<Reading> {
        serde_json::from_slice(bytes).map_err(|error| {
            Categorized::error(
                ErrorKind::OperationFailed,
                format!(
                    "industrial helper line {} is not a reading: {:?} at column {}",
                    line,
                    error.classify(),
                    error.column()
                ),
            )
        })
    }
    fn size(reading: &Reading, deadline: Option<Instant>) -> Result<usize> {
        let mut size = Size { bytes: 0, deadline };
        serde_json::to_writer(&mut size, reading).map_err(|_| {
            if deadline.is_some_and(|until| Instant::now() >= until) {
                return expired();
            }
            failure(
                ErrorKind::OperationFailed,
                "industrial helper serialized reading exceeds 1 MiB batch limit",
            )
        })?;
        Ok(size.bytes)
    }
    pub(super) fn batch(&mut self, deadline: Option<Instant>) -> Result<Vec<Reading>> {
        let mut rows = Vec::new();
        let mut total = 2_usize;
        let mut consumed = 0;
        while let Some(relative) = self
            .pending
            .get(consumed..)
            .and_then(|tail| tail.iter().position(|&b| b == b'\n'))
        {
            time_check(deadline)?;
            if relative > MAX_LINE {
                if !rows.is_empty() {
                    break;
                }
                return Err(failure(
                    ErrorKind::OperationFailed,
                    "industrial helper JSON line exceeds 1 MiB",
                ));
            }
            let separator = usize::from(!rows.is_empty());
            if !rows.is_empty() && total + separator + relative > MAX_LINE {
                break;
            }
            let end = consumed + relative;
            let bytes = self
                .pending
                .get(consumed..end)
                .ok_or_else(|| failure(ErrorKind::Incomplete, "invalid helper line extent"))?;
            let row = match Self::reading(bytes, self.line + u64::try_from(rows.len())? + 1) {
                Ok(row) => row,
                Err(_) if !rows.is_empty() => break,
                Err(error) => return Err(error),
            };
            time_check(deadline)?;
            let size = match Self::size(&row, deadline) {
                Ok(size) => size,
                Err(error) if crate::types::classify(&error).kind == ErrorKind::Timeout => {
                    return Err(error);
                }
                Err(_) if !rows.is_empty() => break,
                Err(error) => return Err(error),
            };
            if total + separator + size > MAX_LINE {
                break;
            }
            total += separator + size;
            consumed = end + 1;
            rows.push(row);
            if rows.len() == MAX_BATCH {
                break;
            }
        }
        if rows.is_empty() && self.pending.len() > MAX_LINE {
            return Err(failure(
                ErrorKind::OperationFailed,
                "industrial helper JSON line exceeds 1 MiB",
            ));
        }
        if !rows.is_empty() {
            time_check(deadline)?;
            // Commit only a fully sized, in-budget prefix. Expiry leaves every
            // raw byte available to final drain, rather than consuming a row.
            self.line += u64::try_from(rows.len())?;
            drop(self.pending.drain(..consumed));
        }
        Ok(rows)
    }
    pub(super) fn drain(&mut self) -> Result<Option<Reading>> {
        if self.pending.is_empty() {
            return Ok(None);
        }
        let at = self
            .pending
            .iter()
            .position(|&b| b == b'\n')
            .ok_or_else(|| {
                failure(
                    ErrorKind::Incomplete,
                    "industrial helper acquisition ended with an incomplete JSON line",
                )
            })?;
        if at > MAX_LINE {
            return Err(failure(
                ErrorKind::OperationFailed,
                "industrial helper JSON line exceeds 1 MiB",
            ));
        }
        let row = Self::reading(
            self.pending
                .get(..at)
                .ok_or_else(|| failure(ErrorKind::Incomplete, "invalid helper line extent"))?,
            self.line + 1,
        )?;
        Self::size(&row, None)?;
        drop(self.pending.drain(..=at));
        self.line += 1;
        Ok(Some(row))
    }
    pub(super) fn eof(&self) -> anyhow::Error {
        failure(
            ErrorKind::Disconnected,
            if self.pending.is_empty() {
                "industrial helper closed stdout"
            } else {
                "industrial helper ended with an incomplete JSON line"
            },
        )
    }
}
pub(super) struct Chunk {
    pub(super) bytes: Vec<u8>,
    pub(super) observed: u64,
}
#[derive(Clone, Copy)]
pub(super) enum End {
    Eof,
    Io,
    Clock,
    Stopped,
    Panic,
}
impl End {
    pub(super) fn error(self) -> Option<anyhow::Error> {
        match self {
            Self::Stopped => None,
            Self::Eof => Some(failure(
                ErrorKind::Disconnected,
                "industrial helper closed stdout",
            )),
            Self::Io => Some(failure(
                ErrorKind::Unavailable,
                "industrial helper stdout read failed",
            )),
            Self::Clock => Some(failure(
                ErrorKind::Unavailable,
                "industrial helper receive clock is unavailable",
            )),
            Self::Panic => Some(failure(
                ErrorKind::OperationFailed,
                "industrial helper reader did not complete normally",
            )),
        }
    }
}
pub(super) enum Delivery {
    Chunk(Chunk),
    End(End),
    Idle,
}
#[derive(Default)]
struct State {
    chunk: Option<Chunk>,
    end: Option<End>,
    stop: bool,
    cutoff: Option<u64>,
    stop_clock_missing: bool,
    finished: bool,
}
#[derive(Default)]
pub(super) struct Mailbox {
    state: Mutex<State>,
    changed: Condvar,
}
impl Mailbox {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    pub(super) fn set_cutoff(&self, end: Option<u64>) {
        let mut state = self.state();
        if let Some(end) = end {
            state.cutoff = Some(state.cutoff.map_or(end, |old| old.min(end)));
        }
        self.changed.notify_all();
    }
    pub(super) fn stop(&self, at: Option<u64>) {
        let mut state = self.state();
        state.stop = true;
        state.stop_clock_missing |= at.is_none();
        if let Some(at) = at {
            state.cutoff = Some(state.cutoff.map_or(at, |old| old.min(at)));
        }
        self.changed.notify_all();
    }
    fn reserve(&self, clock: &impl Fn() -> Option<u64>) -> bool {
        let mut state = self.state();
        while state.chunk.is_some() && !state.stop {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        if state.stop {
            return false;
        }
        let Some(now) = clock() else {
            state.end = Some(End::Clock);
            return false;
        };
        if state.cutoff.is_some_and(|end| now >= end) {
            state.stop = true;
            return false;
        }
        true
    }
    fn publish(&self, chunk: Chunk) -> bool {
        let mut state = self.state();
        if state.stop_clock_missing || state.cutoff.is_some_and(|end| chunk.observed > end) {
            return false;
        }
        // There is one producer and reserve waits BEFORE each read; this slot
        // cannot be filled by another producer during the blocking OS call.
        if state.chunk.is_some() {
            state.end = Some(End::Panic);
            return false;
        }
        state.chunk = Some(chunk);
        self.changed.notify_all();
        true
    }
    fn finish_reader(&self, end: End) {
        let mut state = self.state();
        if state.end.is_none() {
            state.end = Some(end);
        }
        state.finished = true;
        self.changed.notify_all();
    }
    pub(super) fn finished(&self) -> bool {
        self.state().finished
    }
    pub(super) fn terminal(&self) -> Option<End> {
        self.state().end
    }
    pub(super) fn take(&self, deadline: Instant) -> Delivery {
        let mut state = self.state();
        loop {
            if let Some(chunk) = state.chunk.take() {
                self.changed.notify_all();
                return Delivery::Chunk(chunk);
            }
            if let Some(end) = state.end {
                return Delivery::End(end);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Delivery::Idle;
            }
            let (next, _) = self
                .changed
                .wait_timeout(state, left)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state = next;
        }
    }
}
pub(super) fn pump<R: Read + Send + 'static>(
    mut reader: R,
    mailbox: Arc<Mailbox>,
    clock: impl Fn() -> Option<u64> + Send + 'static,
) -> io::Result<thread::JoinHandle<()>> {
    thread::Builder::new()
        .name("industrial-helper-stdout".into())
        .spawn(move || {
            struct Done(Arc<Mailbox>);
            impl Drop for Done {
                fn drop(&mut self) {
                    if !self.0.finished() {
                        self.0.finish_reader(End::Panic);
                    }
                }
            }
            let done = Done(mailbox.clone());
            let end = loop {
                if !mailbox.reserve(&clock) {
                    break End::Stopped;
                }
                let mut bytes = vec![0; CHUNK];
                let count = loop {
                    match reader.read(&mut bytes) {
                        Ok(count) => break Ok(count),
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                            if !mailbox.reserve(&clock) {
                                break Ok(0);
                            }
                        }
                        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => break Ok(0),
                        Err(_) => break Err(()),
                    }
                };
                let observed = clock();
                let state = mailbox.state();
                let stopped = state.stop
                    || state
                        .cutoff
                        .is_some_and(|end| observed.is_some_and(|now| now > end));
                drop(state);
                match count {
                    Ok(0) => break if stopped { End::Stopped } else { End::Eof },
                    Err(()) => break if stopped { End::Stopped } else { End::Io },
                    Ok(count) => {
                        let Some(observed) = observed else {
                            break End::Clock;
                        };
                        if count > bytes.len() {
                            break End::Io;
                        }
                        bytes.truncate(count);
                        if !mailbox.publish(Chunk { bytes, observed }) {
                            break End::Stopped;
                        }
                    }
                }
            };
            drop(reader);
            mailbox.finish_reader(end);
            drop(done);
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::atomic::{AtomicU64, Ordering},
        time::Duration,
    };
    #[test]
    fn absolute_startup_cutoff_requires_a_real_clock_and_never_extends() {
        assert!(startup_admitted(Some(100), Some(99)).unwrap());
        assert!(!startup_admitted(Some(100), Some(100)).unwrap());
        assert!(!startup_admitted(Some(100), Some(101)).unwrap());
        assert_eq!(
            crate::types::classify(&startup_admitted(None, None).unwrap_err()).kind,
            ErrorKind::Unavailable
        );
        let mailbox = Mailbox::default();
        mailbox.set_cutoff(Some(100));
        mailbox.set_cutoff(Some(200));
        assert!(!mailbox.reserve(&|| Some(100)));
    }
    #[test]
    fn parsing_expiry_leaves_the_exact_accepted_prefix_for_drain() {
        let mut buffer = Buffer::default();
        let raw = b"{\"signal\":\"n\",\"value\":18446744073709551617}\n";
        buffer
            .append(&Chunk {
                bytes: raw.to_vec(),
                observed: 42,
            })
            .unwrap();
        let error = buffer.batch(Some(Instant::now())).unwrap_err();
        assert_eq!(crate::types::classify(&error).kind, ErrorKind::Timeout);
        assert_eq!(buffer.pending, raw);
        assert_eq!(buffer.line, 0);
        let row = buffer.drain().unwrap().unwrap();
        assert_eq!(row.value.to_string(), "18446744073709551617");
        assert_eq!(buffer.observed, Some(42));
        assert!(buffer.drain().unwrap().is_none());
        assert_eq!(
            crate::types::classify(&Buffer::size(&row, Some(Instant::now())).unwrap_err()).kind,
            ErrorKind::Timeout
        );
    }
    #[test]
    fn bounded_parser_preserves_precision_prefix_and_chunk_clock() {
        let mut buffer = Buffer::default();
        buffer
            .append(&Chunk {
                bytes: b"{\"signal\":\"n\",\"value\":18446744073709551617}\nprivate-secret\n"
                    .to_vec(),
                observed: 42,
            })
            .unwrap();
        let rows = buffer.batch(None).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].value.to_string(), "18446744073709551617");
        assert_eq!(buffer.observed, Some(42));
        let error = buffer.batch(None).unwrap_err();
        assert!(!error.to_string().contains("private-secret"));
    }
    #[test]
    fn batch_limit_keeps_accepted_rows_and_partial_tail_for_drain() {
        let mut buffer = Buffer::default();
        let mut bytes = Vec::new();
        for i in 0..263 {
            bytes.extend_from_slice(format!("{{\"signal\":\"n\",\"value\":{i}}}\n").as_bytes());
        }
        for chunk in bytes.chunks(CHUNK) {
            buffer
                .append(&Chunk {
                    bytes: chunk.to_vec(),
                    observed: 99,
                })
                .unwrap();
        }
        assert_eq!(buffer.batch(None).unwrap().len(), 256);
        for i in 256..263 {
            assert_eq!(buffer.drain().unwrap().unwrap().value, i);
        }
        buffer
            .append(&Chunk {
                bytes: b"{\"signal\":".to_vec(),
                observed: 100,
            })
            .unwrap();
        assert_eq!(
            crate::types::classify(&buffer.drain().unwrap_err()).kind,
            ErrorKind::Incomplete
        );
    }
    #[test]
    fn normalized_json_and_single_line_limits_are_checked_before_publication() {
        let values = std::iter::repeat_n("1e9", 220_000)
            .collect::<Vec<_>>()
            .join(",");
        let raw = format!("{{\"signal\":\"expanded\",\"value\":[{values}]}}\n");
        let mut buffer = Buffer::default();
        for chunk in raw.as_bytes().chunks(CHUNK) {
            buffer
                .append(&Chunk {
                    bytes: chunk.to_vec(),
                    observed: 1,
                })
                .unwrap();
        }
        assert_eq!(
            crate::types::classify(&buffer.batch(None).unwrap_err()).kind,
            ErrorKind::OperationFailed
        );
    }
    #[test]
    fn one_slot_applies_backpressure_and_stopping_preserves_accepted_clock() {
        let mailbox = Arc::new(Mailbox::default());
        mailbox.set_cutoff(Some(100));
        assert!(mailbox.reserve(&|| Some(10)));
        assert!(mailbox.publish(Chunk {
            bytes: vec![1],
            observed: 20
        }));
        mailbox.stop(Some(25));
        assert!(!mailbox.reserve(&|| Some(30)));
        match mailbox.take(Instant::now()) {
            Delivery::Chunk(chunk) => {
                assert_eq!(chunk.bytes, [1]);
                assert_eq!(chunk.observed, 20);
            }
            _ => panic!("lost accepted chunk"),
        }
        let mailbox = Mailbox::default();
        mailbox.set_cutoff(Some(100));
        mailbox.stop(Some(50));
        assert!(mailbox.publish(Chunk {
            bytes: vec![2],
            observed: 49
        }));
        let later = Mailbox::default();
        later.set_cutoff(Some(50));
        assert!(!later.publish(Chunk {
            bytes: vec![3],
            observed: 51
        }));
    }
    #[test]
    fn a_real_reader_pump_orders_data_before_eof_without_per_read_threads() {
        let mailbox = Arc::new(Mailbox::default());
        let clock = Arc::new(AtomicU64::new(10));
        let ticks = clock;
        let reader = io::Cursor::new(vec![7; CHUNK + 1]);
        let thread = pump(reader, mailbox.clone(), move || {
            Some(ticks.fetch_add(1, Ordering::SeqCst))
        })
        .unwrap();
        let end = Instant::now() + Duration::from_secs(2);
        let mut lengths = Vec::new();
        for _ in 0..2 {
            match mailbox.take(end) {
                Delivery::Chunk(chunk) => lengths.push(chunk.bytes.len()),
                _ => panic!("missing bytes"),
            }
        }
        assert_eq!(lengths, [CHUNK, 1]);
        assert!(matches!(mailbox.take(end), Delivery::End(End::Eof)));
        thread.join().unwrap();
    }
}
