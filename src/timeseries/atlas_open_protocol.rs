//! Finite PF6000 result subscription; one saved event precedes each application ACK.
use super::{Reading, Source, net};
use crate::types::{Categorized, ErrorKind, Usage};
use anyhow::Result;
use serde_json::{Value, json};
use std::{
    io,
    net::TcpStream,
    time::{Duration, Instant},
};

mod codec;
mod config;
pub(crate) use config::Config;

const KEEPALIVE: Duration = Duration::from_secs(5);
const MAX_HOUSEKEEPING: usize = 64;

pub(crate) fn open(config: &Config) -> Result<Box<dyn Source>> {
    config.validate()?;
    if crate::host::monotonic_ns().is_none() {
        return Err(Categorized::error(
            ErrorKind::Unsupported,
            "PF6000 requires a host-wide monotonic receive clock",
        ));
    }
    Ok(Box::new(Client::new(config.clone())))
}

struct Received {
    bytes: Vec<u8>,
    observed: u64,
}
impl Received {
    fn error(&self, error: anyhow::Error) -> anyhow::Error {
        crate::types::with_details(
            error,
            json!({"atlas_open_protocol_frame": {
                "raw_hex":crate::types::hex(&self.bytes),
                "host_observed_ns":self.observed
            }}),
        )
    }
    fn header(&self) -> Result<codec::Header> {
        codec::frame(&self.bytes).map_err(|error| self.error(error))
    }
}

struct Client {
    config: Config,
    owner: Option<Box<dyn net::Transport>>,
    controller: Option<Value>,
    initialized: bool,
    subscribed: bool,
    retired: bool,
    last_activity: Instant,
    keepalive_deadline: Option<Instant>,
    pending: Option<Instant>,
    observed: Option<u64>,
    cutoff: Option<u64>,
}
impl Client {
    fn new(config: Config) -> Self {
        Self {
            config,
            owner: None,
            controller: None,
            initialized: false,
            subscribed: false,
            retired: false,
            last_activity: Instant::now(),
            keepalive_deadline: None,
            pending: None,
            observed: None,
            cutoff: None,
        }
    }
    fn retire(&mut self) {
        self.owner = None;
        self.retired = true;
        self.subscribed = false;
        self.pending = None;
        self.keepalive_deadline = None;
    }
    fn send(&mut self, mid: u16, revision: u16, ack: bool, deadline: Instant) -> Result<()> {
        remaining(deadline)?;
        let bytes = codec::command(mid, revision, ack)?;
        let owner = self.owner.as_deref_mut().ok_or_else(closed)?;
        owner.set_deadline(deadline);
        owner.write_all(&bytes).map_err(|error| io_error(&error))?;
        remaining(deadline)?;
        self.last_activity = Instant::now();
        Ok(())
    }
    fn receive(&mut self, deadline: Instant, idle: bool) -> Result<Option<Received>> {
        let owner = self.owner.as_deref_mut().ok_or_else(closed)?;
        owner.set_deadline(deadline);
        let mut header = [0; 4];
        if !fill(|bytes| owner.read(bytes), &mut header, deadline, idle)? {
            return Ok(None);
        }
        let length = codec::length(header)?;
        let mut bytes = vec![0; length + 1];
        bytes
            .get_mut(..4)
            .ok_or_else(|| malformed("header extent"))?
            .copy_from_slice(&header);
        fill(
            |target| owner.read(target),
            bytes.get_mut(4..).ok_or_else(|| malformed("body extent"))?,
            deadline,
            false,
        )?;
        // Complete-frame receive time precedes decoding, enqueueing, disk work and ACK.
        let observed = crate::host::monotonic_ns().ok_or_else(|| {
            Categorized::error(ErrorKind::Unsupported, "PF6000 receive clock unavailable")
        })?;
        let received = Received { bytes, observed };
        remaining(deadline).map_err(|error| received.error(error))?;
        self.last_activity = Instant::now();
        Ok(Some(received))
    }
    fn keepalive_reply(&mut self, received: &Received) -> Result<()> {
        let deadline = self
            .keepalive_deadline
            .ok_or_else(|| received.error(malformed("unsolicited keepalive response")))?;
        remaining(deadline).map_err(|error| received.error(error))?;
        self.keepalive_deadline = None;
        Ok(())
    }
    fn within_cutoff(&self, observed: u64) -> bool {
        self.cutoff.is_none_or(|cutoff| observed < cutoff)
    }
    fn wait(&mut self, mid: u16, deadline: Instant) -> Result<Option<Value>> {
        for _ in 0..MAX_HOUSEKEEPING {
            let deadline = self
                .keepalive_deadline
                .map_or(deadline, |pending| deadline.min(pending));
            let received = self
                .receive(deadline, false)?
                .ok_or_else(|| malformed("control response missing"))?;
            let header = received.header()?;
            if header.mid == 9999 {
                self.keepalive_reply(&received)?;
                if mid == 9999 {
                    return Ok(None);
                }
                continue;
            }
            if header.mid == 61 {
                return Err(received.error(malformed(
                    "unacknowledged result received during control/finish; no sample or ACK issued",
                )));
            }
            if mid == 1 && header.mid == 2 {
                return codec::startup(&received.bytes)
                    .map(Some)
                    .map_err(|error| received.error(error));
            }
            if (matches!(mid, 60 | 63) && header.mid == 5) || header.mid == 4 {
                codec::accepted_or_error(&received.bytes, mid)
                    .map_err(|error| received.error(error))?;
                return Ok(None);
            }
            return Err(received.error(malformed("unexpected control response MID")));
        }
        Err(malformed(
            "too many housekeeping frames before control reply",
        ))
    }
    fn setup(&mut self, deadline: Instant) -> Result<()> {
        let socket = TcpStream::connect_timeout(&self.config.address, remaining(deadline)?)
            .map_err(|error| io_error(&error))?;
        socket.set_nodelay(true).map_err(|error| io_error(&error))?;
        remaining(deadline)?;
        self.owner = Some(net::plain(socket, deadline));
        self.send(1, 5, false, deadline)?;
        self.controller = self.wait(1, deadline)?;
        if self.controller.is_none() {
            return Err(malformed("startup did not return controller metadata"));
        }
        self.send(60, 3, true, deadline)?;
        drop(self.wait(60, deadline)?);
        remaining(deadline)?;
        self.subscribed = true;
        self.initialized = true;
        Ok(())
    }
    fn may_start_keepalive(&self, deadline: Instant, headroom: Duration) -> Result<bool> {
        if deadline.saturating_duration_since(Instant::now()) < headroom {
            return Ok(false);
        }
        if let Some(cutoff) = self.cutoff {
            let now = crate::host::monotonic_ns().ok_or_else(|| {
                Categorized::error(ErrorKind::Unsupported, "PF6000 keepalive clock unavailable")
            })?;
            // A clipped final read must not turn idle completion into a new control timeout.
            if cutoff.saturating_sub(now) <= 20_000_000 {
                return Ok(false);
            }
        }
        Ok(true)
    }
    fn read_until(
        &mut self,
        deadline: Instant,
        new_request_headroom: Duration,
    ) -> Result<Vec<Reading>> {
        if !self.subscribed {
            self.setup(deadline)?;
        }
        for _ in 0..MAX_HOUSEKEEPING {
            if Instant::now() >= deadline {
                return if self.keepalive_deadline.is_some() {
                    Err(timeout())
                } else {
                    Ok(Vec::new())
                };
            }
            if self.keepalive_deadline.is_none() && self.last_activity.elapsed() >= KEEPALIVE {
                // Defer an unstarted ping to the next read instead of starting it at D's edge.
                // This never relaxes the deadline of a request already on the wire.
                if !self.may_start_keepalive(deadline, new_request_headroom)? {
                    return Ok(Vec::new());
                }
                self.send(9999, 1, false, deadline)?;
                self.keepalive_deadline = Some(deadline);
            }
            let wake = self.keepalive_deadline.map_or_else(
                || deadline.min(self.last_activity + KEEPALIVE),
                |pending| deadline.min(pending),
            );
            let Some(received) = self.receive(wake, self.keepalive_deadline.is_none())? else {
                if wake < deadline && Instant::now() >= wake {
                    continue;
                }
                return Ok(Vec::new());
            };
            if !self.within_cutoff(received.observed) {
                return Err(received.error(timeout()));
            }
            match received.header()?.mid {
                9999 => self.keepalive_reply(&received)?,
                61 => {
                    let controller = self.controller.as_ref().ok_or_else(|| {
                        malformed("result arrived before controller initialization")
                    })?;
                    let value = codec::result(&received.bytes, controller)
                        .map_err(|error| received.error(error))?;
                    remaining(deadline).map_err(|error| received.error(error))?;
                    self.observed = Some(received.observed);
                    self.pending = Some(deadline);
                    return Ok(vec![Reading {
                        signal: self.config.signal.clone(),
                        value,
                        source_time: None,
                    }]);
                }
                4 if self.keepalive_deadline.is_some() => {
                    codec::accepted_or_error(&received.bytes, 9999)
                        .map_err(|error| received.error(error))?;
                }
                _ => return Err(received.error(malformed("unexpected subscription MID"))),
            }
        }
        Err(malformed("too many housekeeping frames in one read"))
    }
}

impl Source for Client {
    fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
        if self.retired {
            return Err(closed());
        }
        if self.pending.is_some() {
            self.retire();
            return Err(malformed(
                "previous result has no matching writer checkpoint",
            ));
        }
        self.observed = None;
        let result = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| Usage("PF6000 timeout too large".into()).into())
            .and_then(|deadline| {
                let deadline = self
                    .keepalive_deadline
                    .map_or(deadline, |pending| deadline.min(pending));
                self.read_until(deadline, (timeout / 4).min(Duration::from_millis(20)))
            });
        if result.is_err() {
            self.retire();
        }
        result
    }
    fn ready(&self) -> bool {
        self.initialized
    }
    fn observed_ns(&self) -> Option<u64> {
        self.observed
    }
    fn set_acquisition_end(&mut self, end_ns: Option<u64>) {
        self.cutoff = end_ns;
    }
    fn checkpoint_deadline(&self) -> Option<Instant> {
        self.pending
    }
    fn checkpointed(&mut self) -> Result<()> {
        let result = (|| {
            let deadline = self
                .pending
                .take()
                .ok_or_else(|| malformed("writer checkpoint has no pending result"))?;
            let deadline = if let Some(cutoff) = self.cutoff {
                // Sample Instant first so translation conservatively clips the socket budget.
                let began = Instant::now();
                let now = crate::host::monotonic_ns().ok_or_else(|| {
                    Categorized::error(ErrorKind::Unsupported, "PF6000 ACK clock unavailable")
                })?;
                let left = cutoff
                    .checked_sub(now)
                    .filter(|left| *left > 0)
                    .ok_or_else(timeout)?;
                deadline.min(began + Duration::from_nanos(left))
            } else {
                deadline
            };
            remaining(deadline)?;
            if self.retired {
                return Err(closed());
            }
            self.send(62, 1, false, deadline)
        })();
        if result.is_err() {
            self.retire();
        }
        result
    }
    fn finish(&mut self, timeout: Duration) -> Result<()> {
        let result = (|| {
            if self.pending.is_some() {
                return Err(malformed(
                    "pending result was not checkpointed; closed without ACK",
                ));
            }
            if self.retired || self.owner.is_none() {
                return Ok(());
            }
            let deadline = Instant::now()
                .checked_add(timeout)
                .ok_or_else(|| Usage("PF6000 finish timeout too large".into()))?;
            if self.keepalive_deadline.is_some() {
                // Only one application request may remain outstanding at a time.
                drop(self.wait(9999, deadline)?);
            }
            remaining(deadline)?;
            self.send(63, 1, false, deadline)?;
            drop(self.wait(63, deadline)?);
            remaining(deadline)?;
            Ok(())
        })();
        self.retire();
        result
    }
}

fn fill(
    mut read: impl FnMut(&mut [u8]) -> io::Result<usize>,
    bytes: &mut [u8],
    deadline: Instant,
    idle: bool,
) -> Result<bool> {
    let mut count = 0;
    while count < bytes.len() {
        if Instant::now() >= deadline {
            if count == 0 && idle {
                return Ok(false);
            }
            return Err(timeout());
        }
        match read(
            bytes
                .get_mut(count..)
                .ok_or_else(|| malformed("receive extent"))?,
        ) {
            Ok(0) => {
                return Err(Categorized::error(
                    ErrorKind::Disconnected,
                    "PF6000 connection closed during frame",
                ));
            }
            Ok(received) => count += received,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error)
                if count == 0
                    && idle
                    && matches!(
                        error.kind(),
                        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                    ) =>
            {
                return Ok(false);
            }
            Err(error) => return Err(io_error(&error)),
        }
        if count < bytes.len() {
            remaining(deadline)?;
        }
    }
    Ok(true)
}
fn remaining(deadline: Instant) -> Result<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        Err(timeout())
    } else {
        Ok(left)
    }
}
fn timeout() -> anyhow::Error {
    Categorized::error(
        ErrorKind::Timeout,
        "PF6000 original operation deadline expired",
    )
}
fn closed() -> anyhow::Error {
    Categorized::error(ErrorKind::Disconnected, "PF6000 source owner retired")
}
fn malformed(message: &str) -> anyhow::Error {
    Categorized::error(ErrorKind::Incomplete, format!("PF6000: {message}"))
}
fn io_error(error: &io::Error) -> anyhow::Error {
    Categorized::error(
        match error.kind() {
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => ErrorKind::Timeout,
            _ => ErrorKind::Disconnected,
        },
        format!("PF6000 transport: {error}"),
    )
}

#[cfg(test)]
mod tests;
