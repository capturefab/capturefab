//! One owned SDK worker and one retained, bounded reply mailbox.
use super::{
    Config, process,
    wire::{self, Descriptor, Failure, Request, Response},
};
use crate::{
    timeseries::{Reading, Source},
    types::{Categorized, ErrorKind},
};
use anyhow::Result;
use std::{
    collections::VecDeque,
    fmt,
    sync::{Arc, Condvar, Mutex, PoisonError, mpsc},
    thread,
    time::{Duration, Instant},
};

fn deadline_error() -> anyhow::Error {
    Categorized::error(
        ErrorKind::Timeout,
        "NI-DAQmx original parent deadline expired",
    )
}

const READ_MARGIN_NS: u64 = 2_000_000;
fn read_admitted(now: u64, cutoff: u64) -> bool {
    now.checked_add(READ_MARGIN_NS)
        .is_some_and(|next| next < cutoff)
}

fn bridge_deadline(until: Instant) -> Result<u64> {
    let host = wire::now()?;
    let left = until
        .checked_duration_since(Instant::now())
        .ok_or_else(deadline_error)?;
    if left.is_zero() {
        return Err(deadline_error());
    }
    host.checked_add(u64::try_from(left.as_nanos())?)
        .ok_or_else(|| wire::invalid("NI-DAQmx deadline overflow"))
}

#[derive(Debug)]
struct SharedFailure(Arc<anyhow::Error>);
impl fmt::Display for SharedFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("NI-DAQmx source retired after failure")
    }
}
impl std::error::Error for SharedFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref().as_ref())
    }
}

#[derive(Clone, Copy)]
enum Kind {
    Open,
    Read,
    Finish,
}
struct Work {
    request: Request,
    kind: Kind,
    deadline: Instant,
    deadline_ns: u64,
    earliest: u64,
    cutoff: Option<u64>,
}
#[derive(Debug)]
struct Accepted {
    reading: Option<Reading>,
    observed: u64,
    terminal: Option<Failure>,
    finished: bool,
    next_read_ns: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::super::{
        Measurement, TerminalConfig,
        config::Timing,
        wire::Pacing,
        wire::{Block, Notice},
    };
    use super::*;
    use crate::types::classify;

    fn config() -> Config {
        Config {
            signal: "voltage".into(),
            measurement: Measurement::Voltage {
                channels: vec!["Dev1/ai0".into(), "Dev1/ai1".into()],
                terminal_config: TerminalConfig::Rse,
                min_volts: -10.0,
                max_volts: 10.0,
            },
            timing: Timing::Continuous {
                rate_hz: 1000.0,
                max_samples_per_channel: 2,
                host_buffer_samples_per_channel: 8,
            },
            cleanup_timeout_ms: 30_000,
            library: None,
        }
    }
    fn descriptor(config: &Config) -> Descriptor {
        Descriptor::for_config(config, config.physical_channels(), Some(999.0)).unwrap()
    }

    fn work(kind: Kind, at: u64) -> Work {
        let deadline_ns = at + 1_000_000_000;
        let request = match kind {
            Kind::Open => Request::Open {
                config: config(),
                deadline_ns,
                cutoff_ns: deadline_ns,
            },
            Kind::Read => Request::Read {
                deadline_ns,
                cutoff_ns: deadline_ns,
            },
            Kind::Finish => Request::Finish { deadline_ns },
        };
        Work {
            request,
            kind,
            deadline: Instant::now() + Duration::from_secs(1),
            deadline_ns,
            earliest: at,
            cutoff: None,
        }
    }
    fn notice(code: i32) -> Notice {
        Notice {
            operation: "ReadAnalogF64".into(),
            status_code: code,
            diagnostic_hex: "7061727469616c".into(),
            diagnostic_truncated: false,
            diagnostic_source: "error_string".into(),
            diagnostic_encoding: "utf8".into(),
            diagnostic_lookup_status: 0,
        }
    }
    fn ready(at: u64) -> Validator {
        let config = config();
        let mut state = Validator {
            descriptor: None,
            config: config.clone(),
            next: 0,
            last_clock: 0,
            next_read_ns: None,
        };
        state
            .accept(
                Response::Ready {
                    descriptor: descriptor(&config),
                    observed_ns: at,
                    notices: Vec::new(),
                },
                &work(Kind::Open, at),
            )
            .unwrap();
        state
    }
    fn block() -> Block {
        Block {
            sample_index: 0,
            samples_per_channel: 1,
            bits_le_hex: "0000000000000080341200000000f87f".into(),
            status_code: -200_284,
        }
    }
    fn native_failure() -> Failure {
        Failure::from_error(
            &Categorized::new(ErrorKind::OperationFailed, "native partial read")
                .hint("original hint")
                .details(serde_json::json!({"ni_daqmx_status":-200_284}))
                .into(),
        )
    }
    fn source() -> OwnedSource {
        OwnedSource {
            config: config(),
            client: None,
            cutoff: None,
            ready: true,
            finished: false,
            observed: None,
            pending: VecDeque::new(),
            failure: None,
            next_read_ns: None,
        }
    }

    #[test]
    fn partial_native_block_survives_finish_with_original_clock_and_first_cause() {
        let at = wire::now().unwrap();
        let mut validator = ready(at);
        let accepted = validator
            .accept(
                Response::Read {
                    pacing: None,
                    observed_ns: at,
                    block: Some(block()),
                    notices: vec![notice(-200_284)],
                    terminal: Some(native_failure()),
                },
                &work(Kind::Read, at),
            )
            .unwrap();
        let mut source = source();
        source.accept(accepted);
        let error = source.finish(Duration::from_millis(1)).unwrap_err();
        let classified = classify(&error);
        assert_eq!(classified.kind, ErrorKind::OperationFailed);
        assert_eq!(classified.hint.as_deref(), Some("original hint"));
        let reading = source.drain().unwrap().unwrap();
        assert_eq!(source.observed_ns(), Some(at));
        assert_eq!(
            reading.value["samples"][0][0]["ieee754_le_hex"],
            "0000000000000080"
        );
        assert_eq!(
            reading.value["samples"][0][1]["ieee754_le_hex"],
            "341200000000f87f"
        );
        assert!(reading.value["samples"][0][1].get("volts").is_none());
        let error = source.drain().unwrap_err();
        assert_eq!(
            classify(&error).details.unwrap()["ni_daqmx_status"],
            -200_284
        );
    }

    #[test]
    fn malformed_order_status_and_clock_never_become_an_admitted_block() {
        let at = wire::now().unwrap();
        for mode in 0..8 {
            let mut validator = ready(at);
            let mut value = block();
            let mut notices = vec![notice(-200_284)];
            let mut terminal = Some(native_failure());
            let mut request = work(Kind::Read, at);
            match mode {
                0 => value.sample_index = 1,
                1 => notices.clear(),
                2 => terminal = None,
                3 => {
                    let _ = value.bits_le_hex.pop();
                }
                4 => request.deadline_ns = at - 1,
                5 => request.cutoff = Some(at - 1),
                6 => request.deadline_ns = at,
                _ => request.cutoff = Some(at),
            }
            let error = validator
                .accept(
                    Response::Read {
                        pacing: None,
                        observed_ns: at,
                        block: Some(value),
                        notices,
                        terminal,
                    },
                    &request,
                )
                .unwrap_err();
            assert_eq!(
                classify(&error).kind,
                if mode < 4 {
                    ErrorKind::Incomplete
                } else {
                    ErrorKind::Timeout
                }
            );
        }
        let config = config();
        let mut validator = Validator {
            descriptor: None,
            config: config.clone(),
            next: 0,
            last_clock: 0,
            next_read_ns: None,
        };
        let error = validator
            .accept(
                Response::Ready {
                    descriptor: descriptor(&config),
                    observed_ns: at,
                    notices: vec![notice(-1)],
                },
                &work(Kind::Open, at),
            )
            .unwrap_err();
        assert_eq!(classify(&error).kind, ErrorKind::Incomplete);
    }

    #[test]
    fn late_waiter_keeps_complete_response_and_cleanup_enrichment_preserves_category() {
        let at = wire::now().unwrap();
        let mut validator = ready(at);
        let accepted = validator
            .accept(
                Response::Read {
                    pacing: None,
                    observed_ns: at,
                    block: Some(block()),
                    notices: vec![notice(-200_284)],
                    terminal: Some(native_failure()),
                },
                &work(Kind::Read, at),
            )
            .unwrap();
        let completed = Instant::now();
        let mut mailbox = Mailbox {
            response: Some((completed, Ok(accepted))),
            done: true,
        };
        assert!(
            mailbox
                .ready(completed.checked_sub(Duration::from_millis(1)).unwrap())
                .is_none()
        );
        let (_, accepted) = mailbox.response.take().unwrap();
        let mut source = source();
        source.accept(accepted.unwrap());
        source.cleanup_failure(
            Categorized::new(ErrorKind::Unavailable, "cleanup failed")
                .details(serde_json::json!({"cleanup":"unconfirmed"}))
                .into(),
            false,
        );
        let classified = classify(&source.failure().unwrap_err());
        assert_eq!(classified.kind, ErrorKind::OperationFailed);
        let details = classified.details.unwrap();
        assert_eq!(details["ni_daqmx_status"], -200_284);
        assert_eq!(
            details["worker_cleanup"]["tag"]["details"]["cleanup"],
            "unconfirmed"
        );
        assert_eq!(
            source.drain().unwrap().unwrap().value["samples_per_channel"],
            1
        );
        assert_eq!(source.observed_ns(), Some(at));
    }

    #[test]
    fn cutoff_poll_margin_is_explicit_and_does_not_admit_a_tiny_tail_rpc() {
        for remaining in [0, 1, READ_MARGIN_NS - 1, READ_MARGIN_NS] {
            assert!(!read_admitted(10, 10 + remaining));
        }
        assert!(read_admitted(10, 11 + READ_MARGIN_NS));
        assert!(!read_admitted(u64::MAX - READ_MARGIN_NS + 1, u64::MAX));
        for software in [false, true] {
            let mut source = source();
            if software {
                source.config.timing = Timing::OnDemand {
                    poll_interval_ms: 250,
                };
            }
            source.cutoff = Some(wire::now().unwrap() + READ_MARGIN_NS);
            let until = Instant::now() + Duration::from_millis(10);
            assert_eq!(source.acquire(until).unwrap().len(), 0);
            assert!(source.client.is_none());
            assert_eq!(source.observed_ns(), None);
        }
    }

    #[test]
    fn zero_budget_lazy_source_never_launches_and_has_no_fabricated_clock() {
        let mut source = open(&config()).unwrap();
        source.set_acquisition_end(Some(0));
        assert_eq!(source.read(Duration::ZERO).unwrap().len(), 0);
        assert!(!source.ready());
        assert_eq!(source.observed_ns(), None);
        source.finish(Duration::from_millis(1)).unwrap();
        assert!(source.drain().unwrap().is_none());
    }

    fn software_ready(at: u64) -> Validator {
        let mut config = config();
        config.timing = Timing::OnDemand {
            poll_interval_ms: 250,
        };
        let mut state = Validator {
            config: config.clone(),
            descriptor: None,
            next: 0,
            last_clock: 0,
            next_read_ns: None,
        };
        state
            .accept(
                Response::Ready {
                    descriptor: Descriptor::for_config(&config, config.physical_channels(), None)
                        .unwrap(),
                    observed_ns: at,
                    notices: Vec::new(),
                },
                &work(Kind::Open, at),
            )
            .unwrap();
        assert_eq!(state.next_read_ns, None);
        state
    }
    fn software_reply(
        start: Option<u64>,
        due: u64,
        observed: u64,
        sample_index: Option<u64>,
    ) -> Response {
        Response::Read {
            observed_ns: observed,
            pacing: Some(Pacing {
                attempt_start_ns: start,
                next_not_before_ns: due,
            }),
            block: sample_index.map(|sample_index| Block {
                sample_index,
                status_code: 0,
                ..block()
            }),
            notices: Vec::new(),
            terminal: None,
        }
    }

    #[test]
    fn software_ready_idle_and_slow_reads_preserve_the_attempt_frontier() {
        let at = wire::now().unwrap() - 1_000_000_000;
        let mut state = software_ready(at);
        let start = at + 1_000_000;
        let due = start + 250_000_000;
        let first = state
            .accept(
                software_reply(Some(start), due, start, Some(0)),
                &work(Kind::Read, at),
            )
            .unwrap();
        assert_eq!(first.next_read_ns, Some(due));
        assert!(first.reading.is_some());
        let idle = state
            .accept(
                software_reply(None, due, at + 2_000_000, None),
                &work(Kind::Read, at + 2_000_000),
            )
            .unwrap();
        assert!(idle.reading.is_none());
        assert_eq!(idle.next_read_ns, Some(due));
        let observed = at + 900_000_000;
        let slow = state
            .accept(
                software_reply(Some(due), due + 250_000_000, observed, Some(1)),
                &work(Kind::Read, due),
            )
            .unwrap();
        assert_eq!(slow.observed, observed);
        assert_eq!(slow.next_read_ns, Some(due + 250_000_000));
        assert!(slow.next_read_ns.unwrap() < observed);
    }

    #[test]
    fn inconsistent_or_missing_software_pacing_never_admits_a_sample() {
        let at = wire::now().unwrap() - 1_000_000_000;
        for mode in 0..10 {
            let mut state = software_ready(at);
            let mut request = work(Kind::Read, at + 10_000_000);
            let start = at + 20_000_000;
            let due = start + 250_000_000;
            let mut response = software_reply(Some(start), due, start, Some(0));
            match mode {
                0 => {
                    let Response::Read { pacing, .. } = &mut response else {
                        unreachable!()
                    };
                    *pacing = None;
                }
                1 => response = software_reply(None, due, start, None),
                2 => request.earliest = start + 1,
                3 => state.next_read_ns = Some(start + 1),
                4 => response = software_reply(Some(start), due + 1, start, Some(0)),
                5 => response = software_reply(Some(start + 1), due + 1, start, Some(0)),
                6 => {
                    state.next_read_ns = Some(due);
                    response = software_reply(None, due, due, None);
                }
                7 => {
                    state.next_read_ns = Some(due);
                    response = software_reply(None, due, start, Some(0));
                }
                8 => response = software_reply(Some(start), due, start, None),
                _ => state = ready(at),
            }
            let error = state.accept(response, &request).unwrap_err();
            assert_eq!(classify(&error).kind, ErrorKind::Incomplete);
        }
    }

    #[test]
    fn waiting_for_software_frontier_obeys_original_budget_without_launching_a_worker() {
        let mut source = source();
        source.config.timing = Timing::OnDemand {
            poll_interval_ms: 250,
        };
        let now = wire::now().unwrap();
        source.next_read_ns = Some(now + 250_000_000);
        source.cutoff = Some(now + 500_000_000);
        let until = Instant::now() + Duration::from_millis(1);
        assert!(source.acquire(until).unwrap().is_empty());
        assert!(source.client.is_none());
        assert_eq!(source.observed_ns(), None);
        assert_eq!(source.next_read_ns, Some(now + 250_000_000));
        assert!(Instant::now() < until + Duration::from_millis(500));
    }

    #[test]
    fn input_status_notices_must_match_the_selected_native_read_family() {
        let at = wire::now().unwrap();
        for (measurement, channels) in [
            ("digital_port", serde_json::json!(["Dev1/port0"])),
            (
                "counter_count",
                serde_json::json!([{"physical":"Dev1/ctr0","edge":"rising","initial_count":0,"direction":"up"}]),
            ),
            (
                "counter_frequency",
                serde_json::json!([{"physical":"Dev1/ctr0","edge":"rising","min_hz":0.0,"max_hz":100.0,"method":"dynamic_averaging","measurement_time_seconds":0.125,"divisor":1}]),
            ),
        ] {
            let c:Config=serde_json::from_value(serde_json::json!({"signal":"input","measurement":measurement,"channels":channels,"timing":"on_demand","poll_interval_ms":250})).unwrap();
            let d = if measurement == "digital_port" {
                Descriptor::for_digital_ports(&c, vec!["cfdp0000".into()], vec![1]).unwrap()
            } else {
                Descriptor::for_config(&c, c.physical_channels(), None).unwrap()
            };
            for correct in [false, true] {
                let mut state = Validator {
                    config: c.clone(),
                    descriptor: Some(d.clone()),
                    next: 0,
                    last_clock: at,
                    next_read_ns: None,
                };
                let mut n = notice(1);
                n.operation = if correct {
                    c.measurement.read_operation()
                } else {
                    "ReadAnalogF64"
                }
                .into();
                let response = Response::Read {
                    observed_ns: at,
                    pacing: Some(Pacing {
                        attempt_start_ns: Some(at),
                        next_not_before_ns: at + 250_000_000,
                    }),
                    block: Some(Block {
                        sample_index: 0,
                        samples_per_channel: 1,
                        bits_le_hex: "0".repeat(c.measurement.word_bytes() * 2),
                        status_code: 1,
                    }),
                    notices: vec![n],
                    terminal: None,
                };
                let result = state.accept(response, &work(Kind::Read, at));
                if correct {
                    assert!(result.unwrap().reading.is_some());
                } else {
                    assert_eq!(classify(&result.unwrap_err()).kind, ErrorKind::Incomplete);
                }
            }
        }
    }

    #[cfg(all(feature = "ni-daqmx", target_os = "linux"))]
    #[test]
    fn expired_unqueued_call_retires_the_idle_reader_and_preserves_unconfirmed_native_cleanup() {
        let mut client = Client::spawn(&config(), Instant::now() + Duration::from_secs(2)).unwrap();
        let mut request = work(Kind::Open, wire::now().unwrap());
        request.deadline = Instant::now()
            .checked_sub(Duration::from_millis(1))
            .unwrap();
        let error = client.call(request).unwrap_err();
        assert_eq!(classify(&error).kind, ErrorKind::Timeout);
        let cleanup = client
            .settle(Instant::now() + Duration::from_secs(2))
            .unwrap_err();
        assert_eq!(classify(&cleanup).kind, ErrorKind::Unavailable);
        assert!(client.pump.is_none());
        assert!(client.calls.is_none());
        assert!(client.collect().is_none());
    }
}
#[derive(Default)]
struct Mailbox {
    response: Option<(Instant, Result<Accepted>)>,
    done: bool,
}
impl Mailbox {
    fn ready(&mut self, until: Instant) -> Option<Result<Accepted>> {
        if self
            .response
            .as_ref()
            .is_some_and(|(completed, _)| *completed <= until)
        {
            return self.response.take().map(|(_, result)| result);
        }
        None
    }
}
#[derive(Default)]
struct Shared {
    mailbox: Mutex<Mailbox>,
    changed: Condvar,
}

struct Validator {
    config: Config,
    descriptor: Option<Descriptor>,
    next: u64,
    last_clock: u64,
    next_read_ns: Option<u64>,
}
impl Validator {
    fn accept(&mut self, response: Response, work: &Work) -> Result<Accepted> {
        if let Response::Failed { error } = response {
            Self::failure(&error)?;
            return Ok(Accepted {
                reading: None,
                observed: self.last_clock,
                terminal: Some(error),
                finished: false,
                next_read_ns: self.next_read_ns,
            });
        }
        let (observed, reading, terminal, finished) = match (work.kind, response) {
            (
                Kind::Open,
                Response::Ready {
                    descriptor,
                    observed_ns,
                    notices,
                },
            ) => {
                if self.descriptor.is_some() {
                    return Err(wire::invalid("NI worker repeated initialization"));
                }
                if notices.iter().any(|notice| notice.status_code <= 0) {
                    return Err(wire::invalid(
                        "NI worker initialization contained a negative status",
                    ));
                }
                let reading =
                    wire::event_reading(&self.config, &descriptor, "configured", &notices)?;
                self.descriptor = Some(descriptor);
                (observed_ns, Some(reading), None, false)
            }
            (
                Kind::Read,
                Response::Read {
                    observed_ns,
                    pacing,
                    block,
                    notices,
                    terminal,
                },
            ) => {
                wire::validate_notices(&notices)?;
                if let Some(error) = &terminal {
                    Self::failure(error)?;
                }
                if terminal.is_none() && notices.iter().any(|notice| notice.status_code < 0) {
                    return Err(wire::invalid("NI worker omitted a terminal read error"));
                }
                let descriptor = self
                    .descriptor
                    .as_ref()
                    .ok_or_else(|| wire::invalid("NI worker read before initialization"))?;
                match (self.config.timing.poll_interval_ns(), pacing) {
                    (None, None) => {}
                    (Some(interval), Some(pacing)) => {
                        if let Some(start) = pacing.attempt_start_ns {
                            if start < work.earliest
                                || start > observed_ns
                                || self.next_read_ns.is_some_and(|next| start < next)
                                || start.checked_add(interval) != Some(pacing.next_not_before_ns)
                                || (block.is_none() && terminal.is_none())
                            {
                                return Err(wire::invalid(
                                    "NI worker returned inconsistent software read pacing",
                                ));
                            }
                        } else if self.next_read_ns != Some(pacing.next_not_before_ns)
                            || observed_ns >= pacing.next_not_before_ns
                            || block.is_some()
                            || !notices.is_empty()
                            || terminal.is_some()
                        {
                            return Err(wire::invalid(
                                "NI worker returned inconsistent software idle pacing",
                            ));
                        }
                        self.next_read_ns = Some(pacing.next_not_before_ns);
                    }
                    _ => {
                        return Err(wire::invalid(
                            "NI worker omitted or added software read pacing",
                        ));
                    }
                }
                let reading = if let Some(block) = block {
                    if block.sample_index != self.next
                        || (block.status_code < 0 && terminal.is_none())
                        || (block.status_code != 0
                            && !notices.iter().any(|notice| {
                                notice.operation == self.config.measurement.read_operation()
                                    && notice.status_code == block.status_code
                            }))
                    {
                        return Err(wire::invalid(
                            "NI worker sample order or partial status is inconsistent",
                        ));
                    }
                    let reading = wire::block_reading(&self.config, descriptor, &block, &notices)?;
                    self.next = self
                        .next
                        .checked_add(u64::from(block.samples_per_channel))
                        .ok_or_else(|| wire::invalid("NI worker sample index overflow"))?;
                    Some(reading)
                } else if !notices.is_empty() {
                    Some(wire::event_reading(
                        &self.config,
                        descriptor,
                        "status",
                        &notices,
                    )?)
                } else {
                    None
                };
                (observed_ns, reading, terminal, false)
            }
            (
                Kind::Finish,
                Response::Finished {
                    observed_ns,
                    notices,
                    terminal,
                },
            ) => {
                wire::validate_notices(&notices)?;
                if let Some(error) = &terminal {
                    Self::failure(error)?;
                }
                if terminal.is_none() && notices.iter().any(|notice| notice.status_code < 0) {
                    return Err(wire::invalid("NI worker omitted a terminal cleanup error"));
                }
                let reading = self
                    .descriptor
                    .as_ref()
                    .map(|descriptor| {
                        wire::event_reading(&self.config, descriptor, "finished", &notices)
                    })
                    .transpose()?;
                (observed_ns, reading, terminal, true)
            }
            _ => {
                return Err(wire::invalid(
                    "NI worker replied with an unexpected operation",
                ));
            }
        };
        let now = wire::now()?;
        if observed < self.last_clock || observed < work.earliest || observed > now {
            return Err(wire::invalid("NI worker returned an invalid receive clock"));
        }
        if observed >= work.deadline_ns || work.cutoff.is_some_and(|cutoff| observed >= cutoff) {
            return Err(deadline_error());
        }
        self.last_clock = observed;
        Ok(Accepted {
            reading,
            observed,
            terminal,
            finished,
            next_read_ns: self.next_read_ns,
        })
    }
    fn failure(error: &Failure) -> Result<()> {
        if error.message.is_empty() || error.message.len() > 16 * 1024 {
            return Err(wire::invalid("NI worker returned an invalid error record"));
        }
        Ok(())
    }
}

struct Client {
    process: process::Process,
    calls: Option<mpsc::SyncSender<Work>>,
    shared: Arc<Shared>,
    pump: Option<thread::JoinHandle<()>>,
    flight: bool,
    forced: bool,
}
impl Client {
    fn spawn(config: &Config, until: Instant) -> Result<Self> {
        let (process, mut output, mut input) = process::Process::spawn(until)?;
        let (calls, receiver) = mpsc::sync_channel::<Work>(1);
        let shared = Arc::new(Shared::default());
        let target = Arc::clone(&shared);
        let mut validator = Validator {
            config: config.clone(),
            descriptor: None,
            next: 0,
            last_clock: 0,
            next_read_ns: None,
        };
        let pump = thread::Builder::new()
            .name("nidaqmx-pipe".into())
            .spawn(move || {
                while let Ok(work) = receiver.recv() {
                    let result = wire::write(&mut input, &work.request)
                        .and_then(|()| wire::read::<Response>(&mut output))
                        .and_then(|response| {
                            response.ok_or_else(|| {
                                Categorized::error(
                                    ErrorKind::WorkerCrashed,
                                    "NI worker closed its private reply pipe",
                                )
                            })
                        })
                        .and_then(|response| validator.accept(response, &work));
                    let done = matches!(work.kind, Kind::Finish) || result.is_err();
                    let mut mailbox = target
                        .mailbox
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner);
                    mailbox.response = Some((Instant::now(), result));
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
            flight: false,
            forced: false,
        })
    }
    fn kill(&mut self) {
        self.forced = true;
        // Close the idle request receiver even when no work was queued, or a
        // completed reply reaches the mailbox after the parent's deadline.
        drop(self.calls.take());
        self.process.terminate();
    }
    fn call(&mut self, work: Work) -> Result<Accepted> {
        let until = work.deadline;
        if let Err(error) = process::deadline(until) {
            self.kill();
            return Err(error);
        }
        if self.flight {
            return Err(wire::invalid(
                "NI worker already has an operation in flight",
            ));
        }
        self.calls
            .as_ref()
            .ok_or_else(|| {
                Categorized::error(
                    ErrorKind::WorkerCrashed,
                    "NI worker request pipe is retired",
                )
            })?
            .try_send(work)
            .map_err(|_| {
                Categorized::error(
                    ErrorKind::WorkerCrashed,
                    "NI worker request pipe is unavailable",
                )
            })?;
        self.flight = true;
        let shared = Arc::clone(&self.shared);
        let mut mailbox = shared
            .mailbox
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        loop {
            if let Some(result) = mailbox.ready(until) {
                self.flight = false;
                drop(mailbox);
                if result.is_err() {
                    self.kill();
                }
                return result;
            }
            if mailbox.done && mailbox.response.is_none() {
                drop(mailbox);
                self.kill();
                return Err(Categorized::error(
                    ErrorKind::WorkerCrashed,
                    "NI worker reply reader stopped",
                ));
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                drop(mailbox);
                self.kill();
                return Err(deadline_error());
            }
            let (next, _) = shared
                .changed
                .wait_timeout(mailbox, left)
                .unwrap_or_else(PoisonError::into_inner);
            mailbox = next;
        }
    }
    fn collect(&mut self) -> Option<Result<Accepted>> {
        let mut mailbox = self
            .shared
            .mailbox
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let (_, result) = mailbox.response.take()?;
        self.flight = false;
        Some(result)
    }
    fn settle(&mut self, until: Instant) -> Result<()> {
        let process = self.process.finish(until);
        while self.pump.as_ref().is_some_and(|pump| !pump.is_finished()) {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(Categorized::new(
                    ErrorKind::Unavailable,
                    "NI worker reply reader completion is unconfirmed",
                )
                .details(serde_json::json!({"cleanup":"unconfirmed"}))
                .into());
            }
            thread::park_timeout(left.min(Duration::from_millis(2)));
        }
        let pump = self.pump.take().map_or(Ok(()), |pump| {
            pump.join().map_err(|_| {
                Categorized::error(ErrorKind::WorkerCrashed, "NI worker reply reader panicked")
            })
        });
        process.and(pump)
    }
}

struct OwnedSource {
    config: Config,
    client: Option<Client>,
    cutoff: Option<u64>,
    ready: bool,
    finished: bool,
    observed: Option<u64>,
    pending: VecDeque<(Reading, u64)>,
    failure: Option<Arc<anyhow::Error>>,
    next_read_ns: Option<u64>,
}
pub(super) fn open(config: &Config) -> Result<Box<dyn Source>> {
    config.validate()?;
    Ok(Box::new(OwnedSource {
        config: config.clone(),
        client: None,
        cutoff: None,
        ready: false,
        finished: false,
        observed: None,
        pending: VecDeque::new(),
        failure: None,
        next_read_ns: None,
    }))
}
impl OwnedSource {
    fn remember(&mut self, error: anyhow::Error) {
        if self.failure.is_none() {
            self.failure = Some(Arc::new(error));
        }
    }
    fn failure(&self) -> Result<()> {
        self.failure
            .as_ref()
            .map_or(Ok(()), |cause| Err(SharedFailure(Arc::clone(cause)).into()))
    }
    fn cleanup_failure(&mut self, error: anyhow::Error, native: bool) {
        if let Some(first) = &self.failure {
            let extra = if native {
                serde_json::json!({"native_cleanup":Failure::from_error(&error)})
            } else {
                serde_json::json!({"worker_cleanup":Failure::from_error(&error)})
            };
            self.failure = Some(Arc::new(crate::types::with_details(
                SharedFailure(Arc::clone(first)).into(),
                extra,
            )));
        } else {
            self.remember(error);
        }
    }
    fn accept(&mut self, packet: Accepted) {
        self.next_read_ns = packet.next_read_ns;
        if let Some(reading) = packet.reading {
            self.pending.push_back((reading, packet.observed));
        }
        if let Some(error) = packet.terminal {
            if packet.finished {
                self.cleanup_failure(error.error(), true);
            } else {
                self.remember(error.error());
            }
        }
        if packet.finished {
            self.finished = true;
        }
    }
    fn pop(&mut self) -> Option<Reading> {
        let (reading, observed) = self.pending.pop_front()?;
        self.observed = Some(observed);
        Some(reading)
    }
    fn acquire(&mut self, until: Instant) -> Result<Vec<Reading>> {
        let cutoff_ns = self
            .cutoff
            .ok_or_else(|| wire::invalid("NI source acquisition cutoff was not installed"))?;
        let now = wire::now()?;
        if now >= cutoff_ns {
            return Ok(Vec::new());
        }
        if let Some(next) = self.next_read_ns.filter(|next| now < *next) {
            let left = until
                .saturating_duration_since(Instant::now())
                .min(Duration::from_nanos(
                    next.min(cutoff_ns).saturating_sub(now),
                ));
            thread::park_timeout(left);
            return Ok(Vec::new());
        }
        if self.ready && !read_admitted(now, cutoff_ns) {
            let left = until
                .saturating_duration_since(Instant::now())
                .min(Duration::from_nanos(cutoff_ns.saturating_sub(now)));
            thread::park_timeout(left);
            return Ok(Vec::new());
        }
        let deadline_ns = bridge_deadline(until)?;
        if self.client.is_none() {
            self.client = Some(Client::spawn(&self.config, until)?);
        }
        let kind = if self.ready { Kind::Read } else { Kind::Open };
        let request = if self.ready {
            Request::Read {
                deadline_ns,
                cutoff_ns,
            }
        } else {
            Request::Open {
                config: self.config.clone(),
                deadline_ns,
                cutoff_ns,
            }
        };
        let packet = self
            .client
            .as_mut()
            .ok_or_else(|| wire::invalid("NI worker missing"))?
            .call(Work {
                request,
                kind,
                deadline: until,
                deadline_ns,
                earliest: wire::now()?,
                cutoff: Some(cutoff_ns),
            })?;
        if matches!(kind, Kind::Open) && packet.terminal.is_none() {
            self.ready = true;
        }
        self.accept(packet);
        if let Some(reading) = self.pop() {
            return Ok(vec![reading]);
        }
        self.failure()?;
        let left = until
            .saturating_duration_since(Instant::now())
            .min(Duration::from_nanos(cutoff_ns.saturating_sub(wire::now()?)));
        thread::park_timeout(left.min(Duration::from_millis(2)));
        Ok(Vec::new())
    }
}
impl Source for OwnedSource {
    fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
        if let Some(reading) = self.pop() {
            return Ok(vec![reading]);
        }
        self.failure()?;
        if self.finished {
            return Err(wire::invalid("NI source is already finished"));
        }
        if timeout.is_zero() {
            return Ok(Vec::new());
        }
        let until = Instant::now()
            .checked_add(timeout)
            .ok_or_else(deadline_error)?;
        match self.acquire(until) {
            Ok(rows) => Ok(rows),
            Err(error) => {
                if self.failure.is_none()
                    && let Some(client) = &mut self.client
                {
                    client.kill();
                }
                self.remember(error);
                self.failure()?;
                Ok(Vec::new())
            }
        }
    }
    fn ready(&self) -> bool {
        self.ready
    }
    fn observed_ns(&self) -> Option<u64> {
        self.observed
    }
    fn buffered_timestamps(&self) -> bool {
        true
    }
    fn set_acquisition_end(&mut self, end: Option<u64>) {
        if let Some(end) = end {
            self.cutoff = Some(self.cutoff.map_or(end, |old| old.min(end)));
        }
    }
    fn finish(&mut self, timeout: Duration) -> Result<()> {
        let timeout = timeout.min(Duration::from_millis(u64::from(
            self.config.cleanup_timeout_ms,
        )));
        let until = Instant::now()
            .checked_add(timeout)
            .ok_or_else(deadline_error)?;
        if let Some(mut client) = self.client.take() {
            if let Some(packet) = client.collect() {
                match packet {
                    Ok(packet) => self.accept(packet),
                    Err(error) => self.remember(error),
                }
            }
            if !client.forced && !client.flight {
                let result = bridge_deadline(until).and_then(|deadline_ns| {
                    client.call(Work {
                        request: Request::Finish { deadline_ns },
                        kind: Kind::Finish,
                        deadline: until,
                        deadline_ns,
                        earliest: wire::now()?,
                        cutoff: None,
                    })
                });
                match result {
                    Ok(packet) => self.accept(packet),
                    Err(error) => self.remember(error),
                }
            } else {
                client.kill();
            }
            if let Err(error) = client.settle(until) {
                self.cleanup_failure(error, false);
            }
            if let Some(packet) = client.collect() {
                match packet {
                    Ok(packet) => self.accept(packet),
                    Err(error) => self.remember(error),
                }
            }
        }
        self.finished = true;
        self.failure()
    }
    fn drain(&mut self) -> Result<Option<Reading>> {
        if let Some(reading) = self.pop() {
            return Ok(Some(reading));
        }
        self.failure()?;
        Ok(None)
    }
}
