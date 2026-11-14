//! LMI `GoPxL` Ethernet ASCII `measurement` polling; one query per poll, nothing else sent.
//! Asynchronous output must be disabled externally; any unsolicited byte fails the poll.
use super::{Reading, Source, net};
use crate::types::{Categorized, ErrorKind, Usage};
use anyhow::Result;
use serde_json::json;
use std::{
    io,
    net::TcpStream,
    time::{Duration, Instant},
};

mod codec;
mod config;
use codec::failure;
pub(crate) use config::Config;

/// Bytes of a failed reply kept in error details.
const EVIDENCE: usize = 4096;

pub(crate) fn open(config: &Config) -> Result<Box<dyn Source>> {
    config.validate()?;
    if crate::host::monotonic_ns().is_none() {
        return Err(Categorized::error(
            ErrorKind::Unsupported,
            "GoPxL requires a host-wide monotonic receive clock",
        ));
    }
    Ok(Box::new(Client {
        config: config.clone(),
        owner: None,
        retired: false,
        polls: 0,
        observed: None,
        cutoff: None,
    }))
}

struct Client {
    config: Config,
    owner: Option<Box<dyn net::Transport>>,
    retired: bool,
    polls: u64,
    observed: Option<u64>,
    cutoff: Option<u64>,
}

/// Attach the received bytes (bounded) to a failure.
fn evidence(error: anyhow::Error, bytes: &[u8]) -> anyhow::Error {
    let kept = bytes.get(..EVIDENCE).unwrap_or(bytes);
    crate::types::with_details(
        error,
        json!({"gopxl_reply_raw":{"hex":crate::types::hex(kept),
            "bytes":bytes.len(),"truncated":kept.len() < bytes.len()}}),
    )
}

impl Client {
    /// Read one reply: bytes up to the terminator, which must end the data received.
    fn reply(&mut self, deadline: Instant) -> Result<Vec<u8>> {
        let terminator = self.config.terminator.bytes();
        let owner = self.owner.as_deref_mut().ok_or_else(closed)?;
        let mut bytes = Vec::new();
        let mut chunk = [0; 4096];
        loop {
            remaining(deadline).map_err(|error| evidence(error, &bytes))?;
            let count = match owner.read(&mut chunk) {
                Ok(0) => {
                    return Err(evidence(
                        failure(
                            ErrorKind::Disconnected,
                            "truncated_reply",
                            "connection closed before the reply terminator",
                        ),
                        &bytes,
                    ));
                }
                Ok(count) => count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(evidence(io_error(&error), &bytes)),
            };
            bytes.extend_from_slice(chunk.get(..count).unwrap_or_default());
            if let Some(end) = bytes
                .windows(terminator.len())
                .position(|window| window == terminator)
            {
                if end > codec::MAX_BODY {
                    return Err(evidence(too_large(), &bytes));
                }
                if bytes.len() != end + terminator.len() {
                    return Err(evidence(
                        failure(
                            ErrorKind::Incomplete,
                            "malformed_reply",
                            "bytes followed the reply terminator; disable asynchronous output",
                        ),
                        &bytes,
                    ));
                }
                bytes.truncate(end);
                return Ok(bytes);
            }
            // No terminator yet: a body within the bound could still end here.
            if bytes.len() >= codec::MAX_BODY + terminator.len() {
                return Err(evidence(too_large(), &bytes));
            }
        }
    }

    fn poll(&mut self, deadline: Instant) -> Result<Vec<Reading>> {
        if self.owner.is_none() {
            let socket = TcpStream::connect_timeout(&self.config.address, remaining(deadline)?)
                .map_err(|error| io_error(&error))?;
            socket.set_nodelay(true).map_err(|error| io_error(&error))?;
            remaining(deadline)?;
            self.owner = Some(net::plain(socket, deadline));
        }
        let ordinal = self.polls.checked_add(1).ok_or_else(|| {
            Categorized::error(ErrorKind::OutOfRange, "GoPxL poll ordinal overflow")
        })?;
        self.polls = ordinal;
        let request = codec::request(&self.config);
        let owner = self.owner.as_deref_mut().ok_or_else(closed)?;
        owner.set_deadline(deadline);
        owner
            .write_all(&request)
            .map_err(|error| io_error(&error))?;
        let body = self.reply(deadline)?;
        // Complete-reply receive time precedes parsing and storage.
        let observed = crate::host::monotonic_ns().ok_or_else(|| {
            Categorized::error(ErrorKind::Unsupported, "GoPxL receive clock unavailable")
        })?;
        if self.cutoff.is_some_and(|cutoff| observed >= cutoff) {
            return Err(failure(
                ErrorKind::Timeout,
                "acquisition_cutoff",
                "reply completed at or after the acquisition cutoff",
            ));
        }
        let parsed = codec::parse(&body, &self.config).map_err(|error| evidence(error, &body))?;
        remaining(deadline)?;
        let readings = self
            .config
            .measurements
            .iter()
            .zip(parsed.values)
            .map(|(measurement, mut value)| {
                value["poll_ordinal"] = json!(ordinal);
                value["host_observed_ns"] = json!(observed);
                value["trailing_delimiter_observed"] = json!(parsed.trailing_observed);
                Reading {
                    signal: measurement.name.clone(),
                    value,
                    source_time: None,
                }
            })
            .collect();
        self.observed = Some(observed);
        Ok(readings)
    }

    fn retire(&mut self) {
        self.owner = None;
        self.retired = true;
    }
}

impl Source for Client {
    fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
        if self.retired {
            return Err(closed());
        }
        self.observed = None;
        let result = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| Usage("GoPxL timeout too large".into()).into())
            .and_then(|deadline| self.poll(deadline));
        if result.is_err() {
            // An unanswered or malformed exchange is never retried or reconnected.
            self.observed = None;
            self.retire();
        }
        result
    }
    fn ready(&self) -> bool {
        self.polls > 0
    }
    fn observed_ns(&self) -> Option<u64> {
        self.observed
    }
    fn set_acquisition_end(&mut self, end_ns: Option<u64>) {
        self.cutoff = end_ns;
    }
    fn finish(&mut self, _timeout: Duration) -> Result<()> {
        // Closing TCP is the only shutdown action; no sensor command follows.
        self.retire();
        Ok(())
    }
}

fn too_large() -> anyhow::Error {
    failure(
        ErrorKind::Incomplete,
        "reply_too_large",
        "reply body exceeds 65536 bytes",
    )
}
fn remaining(deadline: Instant) -> Result<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        Err(failure(
            ErrorKind::Timeout,
            "timeout",
            "original poll deadline expired",
        ))
    } else {
        Ok(left)
    }
}
fn closed() -> anyhow::Error {
    Categorized::error(ErrorKind::Disconnected, "GoPxL source owner retired")
}
fn io_error(error: &io::Error) -> anyhow::Error {
    let (kind, reason) = match error.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => (ErrorKind::Timeout, "timeout"),
        _ => (ErrorKind::Disconnected, "transport"),
    };
    failure(kind, reason, &format!("transport: {error}"))
}

#[cfg(test)]
mod tests;
