//! One retained pipe executor; retirement does not start another USB operation.
use super::{
    Config,
    wire::{self, Command, Reply, Response},
};
use crate::{
    timeseries::native_worker::{Process, WorkerMode},
    types::{Categorized, ErrorKind},
};
use anyhow::Result;
use std::{
    fmt,
    sync::{Arc, Condvar, Mutex, PoisonError, mpsc},
    thread,
    time::{Duration, Instant},
};

fn timeout() -> anyhow::Error {
    Categorized::error(ErrorKind::Timeout, "USB original parent deadline expired")
}
fn cleanup_timeout() -> anyhow::Error {
    Categorized::new(ErrorKind::Timeout, "USB worker cleanup is unconfirmed")
        .details(serde_json::json!({"cleanup":"unconfirmed"}))
        .into()
}
#[derive(Debug)]
struct Saved(Arc<anyhow::Error>);
impl fmt::Display for Saved {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self.0.as_ref(), f)
    }
}
impl std::error::Error for Saved {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref().as_ref())
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Open,
    Send,
    Receive,
    Complete,
    Finish,
}
#[derive(Clone, Copy)]
struct Meta {
    id: u64,
    kind: Kind,
    until: Instant,
    deadline_ns: u64,
    earliest: u64,
    cutoff: Option<u64>,
    credit: usize,
}
struct Work {
    request: wire::Request,
    meta: Meta,
}
struct Delivery {
    completed: Instant,
    reply: Result<Reply>,
}
#[derive(Default)]
struct Mailbox {
    response: Option<Delivery>,
    done: bool,
}
#[derive(Default)]
struct Shared {
    mailbox: Mutex<Mailbox>,
    changed: Condvar,
}
struct Session {
    process: Process,
    calls: Option<mpsc::SyncSender<Work>>,
    shared: Arc<Shared>,
    pump: Option<thread::JoinHandle<()>>,
    pending: Option<Meta>,
    next_id: u64,
    started: bool,
}
fn observed(at: u64, meta: Meta, previous: u64) -> Result<()> {
    if at == 0 || at < meta.earliest || at < previous || at > wire::now()? {
        return Err(wire::invalid("USB worker receive clock is invalid"));
    }
    Ok(())
}
fn admit(reply: &Reply, meta: Meta, completed: Instant, current: Instant) -> Result<()> {
    if completed >= meta.until || current >= meta.until {
        return Err(timeout());
    }
    let observed = match &reply.result {
        Response::Data(value) => Some(value.observed_ns),
        Response::Failed(value) => value.evidence.as_ref().map(|value| value.observed_ns),
        _ => None,
    };
    if observed.is_some_and(|at| at >= meta.deadline_ns || meta.cutoff.is_some_and(|end| at >= end))
    {
        return Err(timeout());
    }
    Ok(())
}
fn validate(reply: &Reply, work: &Work, config: &Config, previous: &mut u64) -> Result<()> {
    reply.validate()?;
    if reply.version != wire::VERSION || reply.request_id != work.meta.id {
        return Err(wire::invalid(
            "USB worker reply identity or version mismatch",
        ));
    }
    match &reply.result {
        Response::Ready(value) if work.meta.kind == Kind::Open => value.descriptor.validate(config),
        Response::QuerySent(_) if work.meta.kind == Kind::Send => Ok(()),
        Response::Data(value) if work.meta.kind == Kind::Receive => {
            wire::decode(&value.payload_base64, 1, work.meta.credit)?;
            observed(value.observed_ns, work.meta, *previous)?;
            *previous = value.observed_ns;
            Ok(())
        }
        Response::Complete(_) if work.meta.kind == Kind::Complete => Ok(()),
        Response::Finished(value) if work.meta.kind == Kind::Finish => {
            if let Some(error) = &value.error {
                error.validate()?;
            }
            if value.cleanup.needs_error() && value.error.is_none() {
                return Err(wire::invalid("USB cleanup uncertainty has no error record"));
            }
            Ok(())
        }
        Response::Failed(value) => {
            value.error.validate()?;
            if value.evidence.is_some() && work.meta.kind != Kind::Receive {
                return Err(wire::invalid(
                    "USB payload evidence belongs only to Receive",
                ));
            }
            if let Some(evidence) = &value.evidence {
                evidence.validate()?;
                if usize::try_from(evidence.payload_prefix_total)? > work.meta.credit {
                    return Err(wire::invalid("USB evidence exceeds its Receive credit"));
                }
                observed(evidence.observed_ns, work.meta, *previous)?;
            }
            Ok(())
        }
        _ => Err(wire::invalid(
            "USB worker returned an unexpected response kind",
        )),
    }
}
impl Session {
    fn spawn(config: &Config, until: Instant) -> Result<Self> {
        let (process, mut output, mut input) = Process::spawn(WorkerMode::UsbTmc, until)?;
        let (calls, receiver) = mpsc::sync_channel::<Work>(1);
        let shared = Arc::new(Shared::default());
        let target = Arc::clone(&shared);
        let selector = config.clone();
        let pump = thread::Builder::new()
            .name("usbtmc-pipe".into())
            .spawn(move || {
                let mut previous = 0;
                while let Ok(work) = receiver.recv() {
                    let reply = wire::write(&mut input, &work.request)
                        .and_then(|()| wire::read::<Reply>(&mut output))
                        .and_then(|value| {
                            value.ok_or_else(|| {
                                Categorized::error(
                                    ErrorKind::WorkerCrashed,
                                    "USB worker closed its private pipe",
                                )
                            })
                        })
                        .and_then(|reply| {
                            validate(&reply, &work, &selector, &mut previous)?;
                            Ok(reply)
                        });
                    let done = reply.is_err() || work.meta.kind == Kind::Finish;
                    let mut mailbox = target
                        .mailbox
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner);
                    // Parent submits only after consuming the preceding response.
                    if mailbox.response.is_some() {
                        mailbox.done = true;
                        target.changed.notify_all();
                        return;
                    }
                    mailbox.response = Some(Delivery {
                        completed: Instant::now(),
                        reply,
                    });
                    mailbox.done = done;
                    target.changed.notify_all();
                    drop(mailbox);
                    if done {
                        return;
                    }
                }
                target
                    .mailbox
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .done = true;
                target.changed.notify_all();
            })?;
        Ok(Self {
            process,
            calls: Some(calls),
            shared,
            pump: Some(pump),
            pending: None,
            next_id: 1,
            started: false,
        })
    }
    fn submit(&mut self, command: Command, mut meta: Meta) -> Result<()> {
        if self.pending.is_some() {
            return Err(wire::invalid("USB request is already pending"));
        }
        if Instant::now() >= meta.until {
            return Err(timeout());
        }
        meta.id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or_else(|| wire::invalid("USB request identity exhausted"))?;
        let request = wire::Request {
            version: wire::VERSION,
            request_id: meta.id,
            command,
        };
        request.validate()?;
        self.calls
            .as_ref()
            .ok_or_else(|| wire::invalid("USB request executor is closed"))?
            .try_send(Work { request, meta })
            .map_err(|_| wire::invalid("USB request executor is unavailable"))?;
        self.pending = Some(meta);
        if meta.kind == Kind::Open {
            self.started = true;
        }
        Ok(())
    }
    fn wait(&mut self, until: Instant) -> Result<(Meta, Delivery)> {
        let shared = Arc::clone(&self.shared);
        let mut mailbox = shared
            .mailbox
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        loop {
            if let Some(delivery) = mailbox.response.take() {
                let meta = self
                    .pending
                    .take()
                    .ok_or_else(|| wire::invalid("USB response has no pending request"))?;
                return Ok((meta, delivery));
            }
            if mailbox.done {
                return Err(Categorized::error(
                    ErrorKind::WorkerCrashed,
                    "USB reply executor stopped",
                ));
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(timeout());
            }
            let (next, _) = shared
                .changed
                .wait_timeout(mailbox, left)
                .unwrap_or_else(PoisonError::into_inner);
            mailbox = next;
        }
    }
    fn acquire(&mut self, command: Command, meta: Meta) -> Result<Reply> {
        self.submit(command, meta)?;
        let (meta, delivery) = match self.wait(meta.until) {
            Ok(delivery) => delivery,
            Err(error) => {
                if crate::types::classify(&error).kind != ErrorKind::Timeout {
                    self.kill();
                }
                return Err(error);
            }
        };
        let reply = match delivery.reply {
            Ok(reply) => reply,
            Err(error) => {
                self.kill();
                return Err(error);
            }
        };
        admit(&reply, meta, delivery.completed, Instant::now())?;
        Ok(reply)
    }
    fn kill(&mut self) {
        drop(self.calls.take());
        self.process.terminate();
    }
    fn settle(&mut self, until: Instant) -> Result<()> {
        let process = self.process.finish(until);
        while self.pump.as_ref().is_some_and(|pump| !pump.is_finished()) {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(cleanup_timeout());
            }
            thread::park_timeout(left.min(Duration::from_millis(2)));
        }
        let pump = self.pump.take().map_or(Ok(()), |pump| {
            pump.join().map_err(|_| {
                Categorized::error(ErrorKind::WorkerCrashed, "USB reply executor panicked")
            })
        });
        process.and(pump)
    }
    fn finish(&mut self, until: Instant) -> Result<()> {
        if !self.started {
            // No Open reached the executor: Finish cannot be the first command.
            self.kill();
            return self.settle(until);
        }
        let exchange = (|| {
            if self.pending.is_some() {
                let (_, delivery) = self.wait(until)?;
                // Late acquisition bytes are permanently discarded; only a
                // structurally valid reply permits further private protocol use.
                delivery.reply?;
            }
            let meta = metadata(Kind::Finish, until, None, 0)?;
            let deadline_ns = meta.deadline_ns;
            self.submit(
                Command::Finish(Box::new(wire::Deadline { deadline_ns })),
                meta,
            )?;
            let (_, delivery) = self.wait(until)?;
            if delivery.completed >= until || Instant::now() >= until {
                return Err(cleanup_timeout());
            }
            delivery.reply
        })();
        drop(self.calls.take());
        if exchange.is_err() {
            self.process.terminate();
        }
        let result = exchange.and_then(|reply| match reply.result {
            Response::Finished(value) => cleanup_result(*value),
            Response::Failed(value) => Err(value.error.error()),
            _ => Err(wire::invalid("USB Finish returned an invalid response")),
        });
        let settled = self.settle(until);
        combine(result, settled)
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.kill();
    }
}
fn cleanup_result(value: wire::Finished) -> Result<()> {
    let wire::Finished { cleanup, error } = value;
    error.map_or(Ok(()), |error| {
        Err(crate::types::with_details(
            error.error(),
            serde_json::json!({"usbtmc_cleanup":cleanup}),
        ))
    })
}
fn combine(primary: Result<()>, secondary: Result<()>) -> Result<()> {
    match (primary, secondary) {
        (Err(first), Err(cleanup)) => {
            let value = crate::types::classify(&first);
            let mut details = value.details.unwrap_or_else(|| serde_json::json!({}));
            if !details.is_object() {
                details = serde_json::json!({"original_details":details});
            }
            details["worker_cleanup"] = serde_json::json!({"message":cleanup.to_string(),"tag":crate::types::error_tag(&cleanup)});
            Err(Categorized {
                kind: value.kind,
                message: first.to_string(),
                hint: value.hint,
                details: Some(details),
            }
            .into())
        }
        (Err(error), _) | (_, Err(error)) => Err(error),
        _ => Ok(()),
    }
}
fn metadata(kind: Kind, until: Instant, cutoff: Option<u64>, credit: usize) -> Result<Meta> {
    let earliest = wire::now()?;
    let left = until
        .checked_duration_since(Instant::now())
        .filter(|left| !left.is_zero())
        .ok_or_else(timeout)?;
    if cutoff.is_some_and(|end| earliest >= end) {
        return Err(timeout());
    }
    let deadline_ns = earliest
        .checked_add(u64::try_from(left.as_nanos())?)
        .ok_or_else(|| wire::invalid("USB deadline overflow"))?;
    Ok(Meta {
        id: 0,
        kind,
        until,
        deadline_ns,
        earliest,
        cutoff,
        credit,
    })
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    New,
    Idle,
    Query,
    Eom,
    Retired,
    Finished,
}
#[derive(Clone, Copy)]
struct Poll {
    until: Instant,
    deadline_ns: u64,
}
pub(crate) struct Client {
    selector: Config,
    session: Option<Session>,
    poll: Option<Poll>,
    cutoff: Option<u64>,
    phase: Phase,
    observed: Option<u64>,
    failure: Option<Arc<anyhow::Error>>,
}
impl Client {
    pub(crate) fn new(selector: &Config) -> Result<Self> {
        selector.validate()?;
        Ok(Self {
            selector: selector.clone(),
            session: None,
            poll: None,
            cutoff: None,
            phase: Phase::New,
            observed: None,
            failure: None,
        })
    }
    fn remember<T>(&mut self, result: Result<T>) -> Result<T> {
        if let Err(error) = result {
            if self.failure.is_none() {
                self.failure = Some(Arc::new(error));
            }
            self.phase = Phase::Retired;
            return Err(Saved(Arc::clone(
                self.failure
                    .as_ref()
                    .ok_or_else(|| wire::invalid("USB failure was not retained"))?,
            ))
            .into());
        }
        result
    }
    fn request(
        &mut self,
        command: Command,
        kind: Kind,
        until: Instant,
        credit: usize,
    ) -> Result<Response> {
        self.check()?;
        let poll = self
            .poll
            .ok_or_else(|| wire::invalid("USB poll has not begun"))?;
        if until != poll.until {
            return Err(wire::invalid(
                "USB poll deadline changed between sibling requests",
            ));
        }
        let mut meta = metadata(kind, until, self.cutoff, credit)?;
        meta.deadline_ns = poll.deadline_ns;
        let result = self
            .session
            .as_mut()
            .ok_or_else(|| wire::invalid("USB worker is not open"))?
            .acquire(command, meta);
        let response = self.remember(result)?.result;
        match response {
            Response::Failed(value) => {
                let mut error = value.error.error();
                if let Some(evidence) = value.evidence {
                    let classified = crate::types::classify(&error);
                    let mut details = classified.details.unwrap_or_else(|| serde_json::json!({}));
                    details["usbtmc_evidence"] = serde_json::to_value(evidence)?;
                    error = Categorized {
                        kind: classified.kind,
                        message: error.to_string(),
                        hint: classified.hint,
                        details: Some(details),
                    }
                    .into();
                }
                self.remember(Err(error))
            }
            response => Ok(response),
        }
    }
    pub(crate) fn begin(&mut self, until: Instant) -> Result<()> {
        self.check()?;
        if !matches!(self.phase, Phase::New | Phase::Idle) {
            return self.remember(Err(wire::invalid("USB preceding query is incomplete")));
        }
        let metadata = metadata(Kind::Open, until, self.cutoff, 0);
        let meta = self.remember(metadata)?;
        self.poll = Some(Poll {
            until,
            deadline_ns: meta.deadline_ns,
        });
        if self.session.is_none() {
            let session = Session::spawn(&self.selector, until);
            self.session = Some(self.remember(session)?);
            let response = self.request(
                Command::Open(Box::new(wire::Open {
                    selector: self.selector.clone(),
                    deadline_ns: meta.deadline_ns,
                    acquisition_end_ns: self.cutoff,
                })),
                Kind::Open,
                until,
                0,
            )?;
            if !matches!(response, Response::Ready(_)) {
                return self.remember(Err(wire::invalid("USB open did not return Ready")));
            }
        }
        self.phase = Phase::Idle;
        Ok(())
    }
    fn deadline(&self, until: Instant) -> Result<u64> {
        self.check()?;
        let poll = self
            .poll
            .ok_or_else(|| wire::invalid("USB poll has not begun"))?;
        if poll.until != until || Instant::now() >= until {
            return Err(timeout());
        }
        wire::check_deadline(poll.deadline_ns, self.cutoff)?;
        Ok(poll.deadline_ns)
    }
    fn checked_deadline(&mut self, until: Instant) -> Result<u64> {
        let result = self.deadline(until);
        self.remember(result)
    }
    pub(crate) fn send_query(&mut self, bytes: &[u8], until: Instant) -> Result<()> {
        if self.phase != Phase::Idle {
            return self.remember(Err(wire::invalid("USB query overlaps another reply")));
        }
        if !(1..=4098).contains(&bytes.len()) {
            return self.remember(Err(wire::invalid("USB query exceeds its bounded size")));
        }
        let deadline_ns = self.checked_deadline(until)?;
        self.request(
            Command::SendQuery(Box::new(wire::SendQuery {
                bytes_base64: wire::encode(bytes),
                deadline_ns,
            })),
            Kind::Send,
            until,
            0,
        )?;
        self.phase = Phase::Query;
        self.observed = None;
        Ok(())
    }
    pub(crate) fn receive(&mut self, bytes: &mut [u8], until: Instant) -> Result<usize> {
        self.check()?;
        if bytes.is_empty() || self.phase == Phase::Eom {
            return Ok(0);
        }
        if self.phase != Phase::Query {
            return self.remember(Err(wire::invalid("USB read has no query")));
        }
        let deadline_ns = self.checked_deadline(until)?;
        let credit = bytes.len().min(wire::MAX_DATA);
        let response = self.request(
            Command::Receive(Box::new(wire::Receive {
                max_bytes: u32::try_from(credit)?,
                deadline_ns,
            })),
            Kind::Receive,
            until,
            credit,
        )?;
        let Response::Data(value) = response else {
            return self.remember(Err(wire::invalid("USB read did not return Data")));
        };
        let payload = wire::decode(&value.payload_base64, 1, credit)?;
        bytes
            .get_mut(..payload.len())
            .ok_or_else(|| wire::invalid("USB reply exceeds its caller buffer"))?
            .copy_from_slice(&payload);
        self.observed = Some(value.observed_ns);
        if value.eom {
            self.phase = Phase::Eom;
        }
        Ok(payload.len())
    }
    pub(crate) const fn ended(&self) -> bool {
        matches!(self.phase, Phase::Eom)
    }
    pub(crate) const fn observed(&self) -> Option<u64> {
        self.observed
    }
    pub(crate) fn complete(&mut self, until: Instant) -> Result<()> {
        if self.phase != Phase::Eom {
            return self.remember(Err(wire::invalid("USB reply ended without EOM")));
        }
        let deadline_ns = self.checked_deadline(until)?;
        self.request(
            Command::Complete(Box::new(wire::Deadline { deadline_ns })),
            Kind::Complete,
            until,
            0,
        )?;
        self.phase = Phase::Idle;
        Ok(())
    }
    pub(crate) fn check(&self) -> Result<()> {
        if let Some(error) = &self.failure {
            return Err(Saved(Arc::clone(error)).into());
        }
        if matches!(self.phase, Phase::Retired | Phase::Finished) {
            return Err(Categorized::error(
                ErrorKind::Disconnected,
                "USB SCPI source is retired",
            ));
        }
        Ok(())
    }
    pub(crate) fn retire(&mut self) {
        if self.phase != Phase::Finished {
            self.phase = Phase::Retired;
        }
    }
    pub(crate) fn set_acquisition_end(&mut self, end: Option<u64>) {
        if self.session.is_some() && self.cutoff != end {
            let _: Result<()> = self.remember(Err(wire::invalid(
                "USB acquisition cutoff changed after Open",
            )));
        }
        self.cutoff = end;
    }
    pub(crate) fn finish(&mut self, timeout: Duration) -> Result<()> {
        if self.phase == Phase::Finished {
            return self
                .failure
                .as_ref()
                .map_or(Ok(()), |error| Err(Saved(Arc::clone(error)).into()));
        }
        let until = Instant::now()
            .checked_add(timeout)
            .ok_or_else(cleanup_timeout)?;
        let cleanup = self
            .session
            .as_mut()
            .map_or(Ok(()), |session| session.finish(until));
        self.phase = Phase::Finished;
        let primary = self
            .failure
            .as_ref()
            .map_or(Ok(()), |error| Err(Saved(Arc::clone(error)).into()));
        let result = combine(primary, cleanup);
        if let Err(error) = result {
            let error = Arc::new(error);
            self.failure = Some(Arc::clone(&error));
            return Err(Saved(error).into());
        }
        Ok(())
    }
}

/// Exercise the real client with short explicit budgets in the fixture process.
#[cfg(feature = "testing")]
pub(super) fn run_probe(input: serde_json::Value) -> Result<serde_json::Value> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Input {
        selector: Config,
        #[serde(deserialize_with = "crate::config_number::unsigned")]
        acquisition_ms: u64,
        #[serde(deserialize_with = "crate::config_number::unsigned")]
        finish_ms: u64,
    }
    fn outcome(result: &Result<()>) -> serde_json::Value {
        match result {
            Ok(()) => serde_json::json!({"ok":true}),
            Err(error) => {
                let classified = crate::types::classify(error);
                let mut value = serde_json::json!({"ok":false,"code":classified.kind.code().0,"message":error.to_string()});
                if let Some(details) = classified.details {
                    value["details"] = details;
                }
                value
            }
        }
    }
    if !input.is_object() {
        return Err(wire::invalid("USB fixture probe input must be an object"));
    }
    let input: Input = serde_json::from_value(input)?;
    if !(1..=5000).contains(&input.acquisition_ms) || !(1..=5000).contains(&input.finish_ms) {
        return Err(wire::invalid(
            "USB fixture probe budget is outside its bound",
        ));
    }
    let mut client = Client::new(&input.selector)?;
    let duration = Duration::from_millis(input.acquisition_ms);
    let until = Instant::now().checked_add(duration).ok_or_else(timeout)?;
    client.set_acquisition_end(Some(
        wire::now()?
            .checked_add(u64::try_from(duration.as_nanos())?)
            .ok_or_else(timeout)?,
    ));
    let mut payload = Vec::new();
    let acquired = (|| {
        client.begin(until)?;
        client.send_query(b"READ?\n", until)?;
        while !client.ended() {
            let mut bytes = [0; 4096];
            let count = client.receive(&mut bytes, until)?;
            if payload
                .len()
                .checked_add(count)
                .is_none_or(|length| length > wire::MAX_DATA)
            {
                return Err(wire::invalid(
                    "USB fixture probe payload exceeded its bound",
                ));
            }
            payload.extend_from_slice(
                bytes
                    .get(..count)
                    .ok_or_else(|| wire::invalid("USB fixture probe receive count is invalid"))?,
            );
        }
        client.complete(until)
    })();
    if acquired.is_err() {
        client.retire();
        payload.clear();
    }
    let finished = client.finish(Duration::from_millis(input.finish_ms));
    let joined = client
        .session
        .as_ref()
        .is_none_or(|session| session.pump.is_none());
    let mut result = serde_json::json!({"acquisition":outcome(&acquired),"finish":outcome(&finished),
        "payload_base64":wire::encode(&payload),"executor_joined":joined});
    if let Some(observed) = client.observed() {
        result["observed_ns"] = observed.into();
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::classify;

    fn config() -> Config {
        Config {
            vendor_id: 1,
            product_id: 0,
            serial_number: "A".into(),
            interface_number: 0,
        }
    }
    fn data(id: u64, at: u64) -> Reply {
        Reply {
            version: wire::VERSION,
            request_id: id,
            result: Response::Data(Box::new(wire::Data {
                payload_base64: "QQ==".into(),
                eom: true,
                observed_ns: at,
            })),
        }
    }
    #[test]
    fn exact_deadline_cutoff_and_late_delivery_never_admit_payload() {
        let start = Instant::now();
        let until = start + Duration::from_secs(1);
        let meta = Meta {
            id: 1,
            kind: Kind::Receive,
            until,
            deadline_ns: 20,
            earliest: 1,
            cutoff: Some(15),
            credit: 1,
        };
        admit(&data(1, 14), meta, start, start).unwrap();
        for at in [15, 20, 21] {
            assert_eq!(
                classify(&admit(&data(1, at), meta, start, start).unwrap_err()).kind,
                ErrorKind::Timeout
            );
        }
        for (completed, current) in [(until, start), (start, until)] {
            assert_eq!(
                classify(&admit(&data(1, 14), meta, completed, current).unwrap_err()).kind,
                ErrorKind::Timeout
            );
        }
        let meta = Meta {
            cutoff: None,
            ..meta
        };
        admit(&data(1, 19), meta, start, start).unwrap();
        assert_eq!(
            classify(&admit(&data(1, 20), meta, start, start).unwrap_err()).kind,
            ErrorKind::Timeout
        );
    }
    #[test]
    fn wrong_identity_kind_credit_and_regressed_clock_are_protocol_errors() {
        let at = wire::now().unwrap();
        let meta = Meta {
            id: 3,
            kind: Kind::Receive,
            until: Instant::now() + Duration::from_secs(1),
            deadline_ns: at + 1_000_000_000,
            earliest: at,
            cutoff: None,
            credit: 1,
        };
        let request = wire::Request {
            version: wire::VERSION,
            request_id: 3,
            command: Command::Receive(Box::new(wire::Receive {
                max_bytes: 1,
                deadline_ns: meta.deadline_ns,
            })),
        };
        let work = Work { request, meta };
        validate(&data(3, at), &work, &config(), &mut 0).unwrap();
        let wrong = Reply {
            version: wire::VERSION,
            request_id: 3,
            result: Response::Complete(wire::Empty),
        };
        for reply in [data(4, at), data(3, at - 1), wrong] {
            assert_eq!(
                classify(&validate(&reply, &work, &config(), &mut 0).unwrap_err()).kind,
                ErrorKind::Incomplete
            );
        }
        let error = wire::Failure::from_error(&wire::invalid("native partial"));
        let evidence = wire::Evidence {
            payload_prefix_base64: "QQ==".into(),
            payload_prefix_total: 1,
            payload_prefix_truncated: false,
            observed_ns: at,
        };
        let reply = Reply {
            version: wire::VERSION,
            request_id: 3,
            result: Response::Failed(Box::new(wire::Failed {
                error,
                evidence: Some(evidence),
            })),
        };
        validate(&reply, &work, &config(), &mut 0).unwrap();
        let oversized = Reply {
            version: wire::VERSION,
            request_id: 3,
            result: Response::Failed(Box::new(wire::Failed {
                error: wire::Failure::from_error(&wire::invalid("native prefix exceeds credit")),
                evidence: Some(wire::Evidence {
                    payload_prefix_base64: "QUI=".into(),
                    payload_prefix_total: 2,
                    payload_prefix_truncated: false,
                    observed_ns: at,
                }),
            })),
        };
        assert_eq!(
            classify(&validate(&oversized, &work, &config(), &mut 0).unwrap_err()).kind,
            ErrorKind::Incomplete
        );
        let wrong_kind = Work {
            meta: Meta {
                kind: Kind::Finish,
                ..meta
            },
            request: wire::Request {
                version: wire::VERSION,
                request_id: 3,
                command: Command::Finish(Box::new(wire::Deadline {
                    deadline_ns: meta.deadline_ns,
                })),
            },
        };
        assert_eq!(
            classify(&validate(&reply, &wrong_kind, &config(), &mut 0).unwrap_err()).kind,
            ErrorKind::Incomplete
        );
        let mut reply = data(3, at);
        if let Response::Data(value) = &mut reply.result {
            value.payload_base64 = "QUI=".into();
        }
        assert_eq!(
            classify(&validate(&reply, &work, &config(), &mut 0).unwrap_err()).kind,
            ErrorKind::Incomplete
        );
        assert_eq!(
            classify(&validate(&data(3, at), &work, &config(), &mut (at + 1)).unwrap_err()).kind,
            ErrorKind::Incomplete
        );
    }
    #[test]
    fn first_error_survives_cleanup_enrichment_and_repeated_finish() {
        let mut client = Client::new(&config()).unwrap();
        let original = Categorized::new(ErrorKind::PermissionDenied, "claim denied")
            .hint("external driver setup")
            .details(serde_json::json!({"native_code":5}));
        let error = client.remember::<()>(Err(original.into())).unwrap_err();
        assert_eq!(classify(&error).kind, ErrorKind::PermissionDenied);
        for _ in 0..2 {
            let error = client.finish(Duration::ZERO).unwrap_err();
            let value = classify(&error);
            assert_eq!(value.kind, ErrorKind::PermissionDenied);
            assert_eq!(value.hint.as_deref(), Some("external driver setup"));
            assert_eq!(value.details.unwrap()["native_code"], 5);
        }
        let original = Categorized::new(ErrorKind::Timeout, "original timeout")
            .details(serde_json::json!({"operation":"receive"}));
        let error = wire::Failure::from_error(
            &Categorized::new(ErrorKind::Unavailable, "unfinished message")
                .details(serde_json::json!({"native_status":128}))
                .into(),
        );
        let cleanup = cleanup_result(wire::Finished {
            cleanup: wire::Cleanup {
                host_resources_released: true,
                native_transfers_settled: true,
                class_abort: wire::ClassAbort::Succeeded,
                message_state: wire::MessageState::Unconfirmed,
            },
            error: Some(error),
        });
        let error = combine(Err(original.into()), cleanup).unwrap_err();
        let value = classify(&error);
        assert_eq!(value.kind, ErrorKind::Timeout);
        let details = value.details.unwrap();
        assert_eq!(details["operation"], "receive");
        assert_eq!(
            details["worker_cleanup"]["tag"]["details"]["native_status"],
            128
        );
        assert_eq!(
            details["worker_cleanup"]["tag"]["details"]["usbtmc_cleanup"]["class_abort"],
            "succeeded"
        );
        assert_eq!(
            details["worker_cleanup"]["tag"]["details"]["usbtmc_cleanup"]["message_state"],
            "unconfirmed"
        );
    }
    #[test]
    fn zero_budget_lazy_transport_has_no_process_or_fabricated_clock() {
        let mut client = Client::new(&config()).unwrap();
        client.set_acquisition_end(Some(0));
        let error = client.begin(Instant::now()).unwrap_err();
        assert_eq!(classify(&error).kind, ErrorKind::Timeout);
        assert!(client.session.is_none());
        assert_eq!(client.observed(), None);
        assert_eq!(
            classify(&client.finish(Duration::ZERO).unwrap_err()).kind,
            ErrorKind::Timeout
        );
    }
}
