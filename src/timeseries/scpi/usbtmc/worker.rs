//! One owned USB state machine. Failed acquisition remains Finish-capable.
use super::{
    Config, codec,
    native::{Backend, Completion, ControlRequest, Direction, Native, Opened, remaining},
    wire,
};
use crate::types::{Categorized, ErrorKind};
use anyhow::Result;
use std::{
    io, thread,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    New,
    Idle,
    Query,
    Ended,
    Retired,
}
struct Worker<B> {
    backend: B,
    phase: Phase,
    descriptor: Option<wire::Descriptor>,
    cutoff: Option<u64>,
    next_tag: u8,
    out_tag: Option<u8>,
    in_tag: Option<u8>,
    input_capacity: usize,
    started: bool,
    eom: bool,
    query_deadline: Option<u64>,
    input_halted: bool,
    output_halted: bool,
    evidence: Option<wire::Evidence>,
}
fn timeout() -> anyhow::Error {
    Categorized::error(
        ErrorKind::Timeout,
        "USB original operation deadline expired",
    )
}
fn halted(status: &Result<()>) -> bool {
    status
        .as_ref()
        .err()
        .and_then(|error| crate::types::classify(error).details)
        .is_some_and(|details| details["nusb_transfer_status"] == "Stall")
}
fn deadline(deadline_ns: u64, cutoff: Option<u64>) -> Result<Instant> {
    let sampled = Instant::now();
    let end = cutoff.map_or(deadline_ns, |end| end.min(deadline_ns));
    let left = end
        .checked_sub(wire::now()?)
        .filter(|left| *left > 0)
        .ok_or_else(timeout)?;
    sampled
        .checked_add(Duration::from_nanos(left))
        .ok_or_else(timeout)
}
fn checked<T>(end: u64, cutoff: Option<u64>, operation: impl FnOnce() -> Result<T>) -> Result<T> {
    wire::check_deadline(end, cutoff)?;
    let value = operation()?;
    wire::check_deadline(end, cutoff)?;
    Ok(value)
}
fn pause(end: Instant) -> Result<()> {
    thread::sleep(remaining(end)?.min(Duration::from_millis(1)));
    remaining(end)?;
    Ok(())
}
fn descriptor(opened: Opened, capabilities: &[u8]) -> wire::Descriptor {
    wire::Descriptor {
        vendor_id: opened.vendor_id,
        product_id: opened.product_id,
        serial_number: opened.serial_number,
        interface_number: opened.interface_number,
        configuration: opened.configuration,
        alternate_setting: opened.alternate_setting,
        class: opened.class,
        subclass: opened.subclass,
        protocol: opened.protocol,
        bulk_in: opened.bulk_in,
        bulk_out: opened.bulk_out,
        interrupt_in: opened.interrupt_in,
        capabilities_base64: wire::encode(capabilities),
    }
}
impl<B: Backend> Worker<B> {
    fn new(backend: B) -> Self {
        Self {
            backend,
            phase: Phase::New,
            descriptor: None,
            cutoff: None,
            next_tag: 1,
            out_tag: None,
            in_tag: None,
            input_capacity: 0,
            started: false,
            eom: false,
            query_deadline: None,
            input_halted: false,
            output_halted: false,
            evidence: None,
        }
    }
    fn tag(&mut self) -> u8 {
        let tag = self.next_tag;
        self.next_tag = codec::advance(tag);
        tag
    }
    fn valid_command(&self, command: &wire::Command) -> bool {
        match command {
            wire::Command::Open(_) => self.phase == Phase::New,
            wire::Command::SendQuery(_) => self.phase == Phase::Idle,
            wire::Command::Receive(_) => self.phase == Phase::Query,
            wire::Command::Complete(_) => self.phase == Phase::Ended,
            wire::Command::Finish(_) => self.phase != Phase::New,
        }
    }
    fn open(&mut self, selector: &Config, end: u64, cutoff: Option<u64>) -> Result<wire::Response> {
        self.cutoff = cutoff;
        let d = deadline(end, self.cutoff)?;
        let opened = checked(end, self.cutoff, || self.backend.open(selector, d))?;
        let caps = checked(end, self.cutoff, || {
            self.backend.control_in(
                ControlRequest {
                    request_type: 0xa1,
                    request: 7,
                    value: 0,
                    index: u16::from(opened.interface_number),
                    length: 24,
                },
                d,
            )
        })?;
        let actual = descriptor(opened, &caps);
        actual.validate(selector)?;
        wire::check_deadline(end, self.cutoff)?;
        self.descriptor = Some(actual.clone());
        self.phase = Phase::Idle;
        Ok(wire::Response::Ready(Box::new(wire::Ready {
            descriptor: actual,
        })))
    }
    fn output(&mut self, bytes: Vec<u8>, tag: u8, end: u64, d: Instant) -> Result<()> {
        let count = bytes.len();
        self.out_tag = Some(tag);
        checked(end, self.cutoff, || self.backend.submit_out(bytes, d))?;
        wire::check_deadline(end, self.cutoff)?;
        let done = self.backend.wait(Direction::Out, d)?.ok_or_else(timeout)?;
        self.output_halted |= halted(&done.status);
        let exact = done.status.is_ok()
            && done.transferred == count
            && done.data.is_empty()
            && self.backend.pending(Direction::Out) == 0;
        if exact {
            self.out_tag = None;
        }
        done.status?;
        if !exact {
            return Err(wire::invalid(
                "USB command transfer did not complete exactly",
            ));
        }
        wire::check_deadline(end, self.cutoff)?;
        Ok(())
    }
    fn send(&mut self, bytes: &[u8], end: u64) -> Result<wire::Response> {
        let d = deadline(end, self.cutoff)?;
        self.query_deadline = Some(end);
        let tag = self.tag();
        let raw = codec::command(tag, bytes)?;
        self.started = true;
        self.eom = false;
        self.output(raw, tag, end, d)?;
        self.phase = Phase::Query;
        Ok(wire::Response::QuerySent(wire::Empty))
    }
    fn prefix(&mut self, done: &Completion, tag: u8, maximum: u32) {
        if done.observed_ns == 0 || done.transferred != done.data.len() {
            return;
        }
        if let Some(prefix) = codec::trusted_prefix(&done.data, tag, maximum) {
            self.evidence = Some(wire::Evidence {
                payload_prefix_base64: wire::encode(
                    prefix.get(..prefix.len().min(4096)).unwrap_or_default(),
                ),
                payload_prefix_total: u32::try_from(prefix.len()).unwrap_or(maximum),
                payload_prefix_truncated: prefix.len() > 4096,
                observed_ns: done.observed_ns,
            });
        }
    }
    fn receive(&mut self, maximum: u32, end: u64) -> Result<wire::Response> {
        if self.query_deadline != Some(end) {
            return Err(wire::invalid("USB query deadline changed before Receive"));
        }
        let d = deadline(end, self.cutoff)?;
        let packet = self
            .descriptor
            .as_ref()
            .ok_or_else(|| wire::invalid("USB descriptor is missing"))?
            .bulk_in
            .max_packet_size;
        let tag = self.tag();
        self.input_capacity = codec::capacity(maximum, packet)?;
        // The read is in progress as soon as its request header is ACKed. Keep
        // this identity even if an OUT error prevents posting the IN request.
        self.in_tag = Some(tag);
        self.output(codec::request(tag, maximum)?, tag, end, d)?;
        checked(end, self.cutoff, || {
            self.backend.submit_in(self.input_capacity, d)
        })?;
        // Preserve qualified error-prefix data before inspecting native status.
        let done = self.backend.wait(Direction::In, d)?.ok_or_else(timeout)?;
        self.prefix(&done, tag, maximum);
        self.input_halted |= halted(&done.status);
        if done.status.is_ok() && done.short_packet {
            self.in_tag = None;
        }
        done.status?;
        wire::check_deadline(end, self.cutoff)?;
        if done.observed_ns == 0
            || done.observed_ns >= end
            || self.cutoff.is_some_and(|cutoff| done.observed_ns >= cutoff)
            || done.observed_ns > wire::now()?
            || done.transferred != done.data.len()
            || done.transferred > self.input_capacity
            || self.backend.pending(Direction::In) != 0
        {
            return Err(wire::invalid(
                "USB input native completion/clock is invalid",
            ));
        }
        let chunk = codec::response(&done.data, done.short_packet, tag, maximum, packet)?;
        let response = wire::Response::Data(Box::new(wire::Data {
            payload_base64: wire::encode(chunk.payload),
            eom: chunk.eom,
            observed_ns: done.observed_ns,
        }));
        self.in_tag = None;
        self.eom = chunk.eom;
        if self.eom {
            self.phase = Phase::Ended;
        }
        self.evidence = None;
        wire::check_deadline(end, self.cutoff)?;
        Ok(response)
    }
    fn complete(&mut self, end: u64) -> Result<wire::Response> {
        if self.query_deadline != Some(end) {
            return Err(wire::invalid("USB query deadline changed before Complete"));
        }
        wire::check_deadline(end, self.cutoff)?;
        if !self.eom
            || self.in_tag.is_some()
            || self.out_tag.is_some()
            || self.backend.pending(Direction::In) != 0
            || self.backend.pending(Direction::Out) != 0
        {
            return Err(wire::invalid("USB Complete requires already observed EOM"));
        }
        self.phase = Phase::Idle;
        Ok(wire::Response::Complete(wire::Empty))
    }
    fn control(
        &mut self,
        request: u8,
        value: u16,
        endpoint: u8,
        length: u16,
        d: Instant,
    ) -> Result<Vec<u8>> {
        remaining(d)?;
        let result = self.backend.control_in(
            ControlRequest {
                request_type: 0xa2,
                request,
                value,
                index: u16::from(endpoint),
                length,
            },
            d,
        )?;
        remaining(d)?;
        Ok(result)
    }
    fn drain_cancelled(&mut self, direction: Direction, d: Instant) -> Result<()> {
        while self.backend.pending(direction) != 0 {
            remaining(d)?;
            let _ = self.backend.wait(direction, d)?.ok_or_else(timeout)?;
        }
        remaining(d)?;
        Ok(())
    }
    fn cancel(&mut self, direction: Direction, d: Instant) -> Result<()> {
        if self.backend.pending(direction) != 0 {
            self.backend.cancel(direction, d)?;
            self.drain_cancelled(direction, d)?;
        }
        Ok(())
    }
    fn abort_out(&mut self, tag: u8, endpoint: u8, d: Instant) -> Result<()> {
        self.cancel(Direction::Out, d)?;
        codec::initiate(&self.control(1, u16::from(tag), endpoint, 2, d)?, tag)?;
        loop {
            let bytes = self.control(2, 0, endpoint, 8, d)?;
            let (done, _) = codec::abort_status(&bytes, false)?;
            if done {
                break;
            }
            pause(d)?;
        }
        self.backend.clear_halt(Direction::Out, d)?;
        self.output_halted = false;
        remaining(d)?;
        self.out_tag = None;
        Ok(())
    }
    fn drain_short(&mut self, d: Instant) -> Result<()> {
        loop {
            remaining(d)?;
            if self.backend.pending(Direction::In) == 0 {
                if self.input_capacity == 0 {
                    return Err(wire::invalid("missing USB abort drain capacity"));
                }
                self.backend.submit_in(self.input_capacity, d)?;
            }
            let done = self.backend.wait(Direction::In, d)?.ok_or_else(timeout)?;
            done.status?;
            if done.transferred != done.data.len() || done.transferred > self.input_capacity {
                return Err(wire::invalid("invalid USB abort drain completion"));
            }
            if done.short_packet {
                return Ok(());
            }
        }
    }
    fn abort_in(&mut self, tag: u8, endpoint: u8, d: Instant) -> Result<()> {
        // A pending response-request OUT IRP must be retired first, while IN
        // remains posted until the class abort has been initiated.
        self.cancel(Direction::Out, d)?;
        codec::initiate(&self.control(3, u16::from(tag), endpoint, 2, d)?, tag)?;
        self.drain_short(d)?;
        loop {
            let bytes = self.control(4, 0, endpoint, 8, d)?;
            let (done, fifo) = codec::abort_status(&bytes, true)?;
            if done {
                break;
            }
            if fifo {
                self.drain_short(d)?;
            }
            pause(d)?;
        }
        self.in_tag = None;
        Ok(())
    }
    fn cleanup(&mut self, end: u64) -> wire::Response {
        let message_state = if !self.started {
            wire::MessageState::NotStarted
        } else if self.eom {
            wire::MessageState::CompletedEom
        } else {
            wire::MessageState::Unconfirmed
        };
        let pending = self.out_tag.is_some() || self.in_tag.is_some();
        let mut cleanup = wire::Cleanup {
            host_resources_released: false,
            native_transfers_settled: self.backend.pending(Direction::In) == 0
                && self.backend.pending(Direction::Out) == 0,
            class_abort: if pending {
                wire::ClassAbort::Unconfirmed
            } else {
                wire::ClassAbort::NotNeeded
            },
            message_state,
        };
        let mut first = None;
        let d = match deadline(end, None) {
            Ok(d) => Some(d),
            Err(error) => {
                first = Some(wire::Failure::from_error(&error));
                None
            }
        };
        if let Some(d) = d {
            let needs_abort = self.out_tag.is_some() || self.in_tag.is_some();
            if needs_abort {
                cleanup.class_abort = wire::ClassAbort::Unconfirmed;
                let abort = (|| -> Result<()> {
                    let descriptor = self
                        .descriptor
                        .as_ref()
                        .ok_or_else(|| wire::invalid("USB cleanup descriptor is missing"))?;
                    let input = descriptor.bulk_in.address;
                    let output = descriptor.bulk_out.address;
                    if let Some(tag) = self.out_tag {
                        self.abort_out(tag, output, d)?;
                    }
                    if let Some(tag) = self.in_tag {
                        self.abort_in(tag, input, d)?;
                    }
                    Ok(())
                })();
                match abort {
                    Ok(()) => cleanup.class_abort = wire::ClassAbort::Succeeded,
                    Err(error) => {
                        cleanup.class_abort =
                            if crate::types::classify(&error).kind == ErrorKind::Timeout {
                                wire::ClassAbort::Unconfirmed
                            } else {
                                wire::ClassAbort::Failed
                            };
                        first = Some(wire::Failure::from_error(&error));
                    }
                }
            }
            // If normative abort failed, host cancellation is only resource
            // cleanup. Never claim it restored instrument message state.
            for direction in [Direction::Out, Direction::In] {
                if let Err(error) = self.cancel(direction, d) {
                    first.get_or_insert_with(|| wire::Failure::from_error(&error));
                }
            }
            for (direction, stalled) in [
                (Direction::Out, self.output_halted),
                (Direction::In, self.input_halted),
            ] {
                if stalled
                    && self.backend.pending(direction) == 0
                    && let Err(error) = self.backend.clear_halt(direction, d)
                {
                    first.get_or_insert_with(|| wire::Failure::from_error(&error));
                }
            }
            cleanup.native_transfers_settled = self.backend.pending(Direction::In) == 0
                && self.backend.pending(Direction::Out) == 0;
            match self.backend.release(d) {
                Ok(()) => cleanup.host_resources_released = true,
                Err(error) => {
                    first.get_or_insert_with(|| wire::Failure::from_error(&error));
                }
            }
        }
        if cleanup.needs_error() && first.is_none() {
            first = Some(wire::Failure::from_error(&Categorized::error(
                ErrorKind::Unavailable,
                "USB cleanup or unfinished-message synchronization remains unconfirmed",
            )));
        }
        wire::Response::Finished(Box::new(wire::Finished {
            cleanup,
            error: first,
        }))
    }
    fn apply(&mut self, command: wire::Command) -> wire::Response {
        self.evidence = None;
        let result = match command {
            wire::Command::Open(value) => {
                self.open(&value.selector, value.deadline_ns, value.acquisition_end_ns)
            }
            wire::Command::SendQuery(value) => wire::decode(&value.bytes_base64, 1, 4098)
                .and_then(|bytes| self.send(&bytes, value.deadline_ns)),
            wire::Command::Receive(value) => self.receive(value.max_bytes, value.deadline_ns),
            wire::Command::Complete(value) => self.complete(value.deadline_ns),
            wire::Command::Finish(value) => return self.cleanup(value.deadline_ns),
        };
        match result {
            Ok(response) => response,
            Err(error) => {
                self.phase = Phase::Retired;
                wire::Response::Failed(Box::new(wire::Failed {
                    error: wire::Failure::from_error(&error),
                    evidence: self.evidence.take(),
                }))
            }
        }
    }
}
pub(crate) fn run_native() -> Result<()> {
    run_with_backend(Native::default())
}
pub(crate) fn run_with_backend<B: Backend>(backend: B) -> Result<()> {
    let mut output = crate::session::protocol_output()?;
    let input = io::stdin();
    let mut input = input.lock();
    let mut worker = Worker::new(backend);
    let mut expected = Some(1u64);
    while let Some(request) = wire::read::<wire::Request>(&mut input)? {
        request.validate()?;
        if expected != Some(request.request_id) || !worker.valid_command(&request.command) {
            return Err(wire::invalid(
                "USB IPC identity or state transition is invalid",
            ));
        }
        expected = request.request_id.checked_add(1);
        let finish = matches!(request.command, wire::Command::Finish(_));
        let reply = wire::Reply {
            version: wire::VERSION,
            request_id: request.request_id,
            result: worker.apply(request.command),
        };
        reply.validate()?;
        wire::write(&mut output, &reply)?;
        if finish {
            return Ok(());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct NoUsb {
        calls: Vec<&'static str>,
        fail_release: bool,
    }
    impl Backend for NoUsb {
        fn open(&mut self, _: &Config, _: Instant) -> Result<Opened> {
            self.calls.push("open");
            Err(wire::invalid("unexpected USB open"))
        }
        fn control_in(&mut self, _: ControlRequest, _: Instant) -> Result<Vec<u8>> {
            self.calls.push("control");
            Err(wire::invalid("unexpected USB control"))
        }
        fn submit_out(&mut self, _: Vec<u8>, _: Instant) -> Result<()> {
            self.calls.push("out");
            Err(wire::invalid("unexpected USB output"))
        }
        fn submit_in(&mut self, _: usize, _: Instant) -> Result<()> {
            self.calls.push("in");
            Err(wire::invalid("unexpected USB input"))
        }
        fn wait(&mut self, _: Direction, _: Instant) -> Result<Option<Completion>> {
            self.calls.push("wait");
            Err(wire::invalid("unexpected USB wait"))
        }
        fn pending(&self, _: Direction) -> usize {
            0
        }
        fn cancel(&mut self, _: Direction, _: Instant) -> Result<()> {
            self.calls.push("cancel");
            Err(wire::invalid("unexpected USB cancel"))
        }
        fn clear_halt(&mut self, _: Direction, _: Instant) -> Result<()> {
            self.calls.push("halt");
            Err(wire::invalid("unexpected USB halt clear"))
        }
        fn release(&mut self, _: Instant) -> Result<()> {
            self.calls.push("release");
            if self.fail_release {
                Err(Categorized::error(
                    ErrorKind::Unavailable,
                    "fixture release failed",
                ))
            } else {
                Ok(())
            }
        }
    }
    fn future() -> u64 {
        wire::now().unwrap().checked_add(30_000_000_000).unwrap()
    }
    #[test]
    fn expired_open_has_no_usb_effect_and_keeps_finish_available() {
        let literal: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../tests/fixtures/usbtmc/selector-control.json"
        ))
        .unwrap();
        let selector: Config = serde_json::from_value(literal["selector"].clone()).unwrap();
        let mut worker = Worker::new(NoUsb::default());
        let reply = worker.apply(wire::Command::Open(Box::new(wire::Open {
            selector,
            deadline_ns: 1,
            acquisition_end_ns: None,
        })));
        let wire::Response::Failed(failure) = reply else {
            panic!("expected expired Open failure");
        };
        assert_eq!(failure.error.tag.code, "timeout");
        assert_eq!(worker.backend.calls.len(), 0);
        assert!(
            worker.valid_command(&wire::Command::Finish(Box::new(wire::Deadline {
                deadline_ns: future()
            })))
        );
        let wire::Response::Finished(result) = worker.cleanup(future()) else {
            panic!("expected Finish");
        };
        assert!(result.cleanup.host_resources_released);
        assert!(result.error.is_none());
        assert_eq!(worker.backend.calls, ["release"]);
    }
    #[test]
    fn eom_finish_only_releases_and_keeps_cleanup_failure() {
        let mut worker = Worker::new(NoUsb {
            calls: Vec::new(),
            fail_release: true,
        });
        worker.started = true;
        worker.eom = true;
        let wire::Response::Finished(result) = worker.cleanup(future()) else {
            panic!("expected Finish");
        };
        assert_eq!(worker.backend.calls, ["release"]);
        assert!(result.cleanup.native_transfers_settled);
        assert!(!result.cleanup.host_resources_released);
        assert!(matches!(
            result.cleanup.message_state,
            wire::MessageState::CompletedEom
        ));
        assert_eq!(result.error.unwrap().message, "fixture release failed");
    }
    #[test]
    fn clean_host_release_does_not_claim_unfinished_message_synchronization() {
        let mut worker = Worker::new(NoUsb::default());
        worker.started = true;
        worker.phase = Phase::Retired;
        let wire::Response::Finished(result) = worker.cleanup(future()) else {
            panic!("expected Finish");
        };
        assert_eq!(worker.backend.calls, ["release"]);
        assert!(result.cleanup.host_resources_released && result.cleanup.native_transfers_settled);
        assert!(matches!(
            result.cleanup.class_abort,
            wire::ClassAbort::NotNeeded
        ));
        assert!(matches!(
            result.cleanup.message_state,
            wire::MessageState::Unconfirmed
        ));
        assert_eq!(result.error.unwrap().tag.code, "unavailable");
    }
    #[test]
    fn query_deadline_cannot_change_before_receive_or_complete() {
        let end = future();
        for complete in [false, true] {
            let mut worker = Worker::new(NoUsb::default());
            worker.query_deadline = Some(end);
            worker.eom = complete;
            worker.phase = if complete { Phase::Ended } else { Phase::Query };
            let command = if complete {
                wire::Command::Complete(Box::new(wire::Deadline {
                    deadline_ns: end + 1,
                }))
            } else {
                wire::Command::Receive(Box::new(wire::Receive {
                    max_bytes: 32,
                    deadline_ns: end + 1,
                }))
            };
            let wire::Response::Failed(result) = worker.apply(command) else {
                panic!("expected changed-deadline failure");
            };
            assert_eq!(result.error.tag.code, "incomplete");
            assert_eq!(worker.backend.calls.len(), 0);
        }
    }
}
