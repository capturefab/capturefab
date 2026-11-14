use super::*;
use std::{
    io::Write,
    sync::{Arc, Mutex},
    thread,
};

fn corpus() -> Value {
    serde_json::from_str(include_str!(
        "../../../tests/fixtures/atlas_open_protocol/pf6000.json"
    ))
    .unwrap()
}
fn literal(group: &str, name: &str) -> Vec<u8> {
    let corpus = corpus();
    let hex = corpus[group]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == name)
        .unwrap()["wire_hex"]
        .as_str()
        .unwrap();
    let (pairs, tail) = hex.as_bytes().as_chunks::<2>();
    assert_eq!(tail, b"");
    pairs
        .iter()
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}
fn assert_idle(readings: &[Reading]) {
    let [] = readings else {
        panic!("unexpected idle readings: {readings:?}")
    };
}
fn event() -> Vec<u8> {
    literal("valid_results", "result_003")
}
#[derive(Default)]
struct State {
    output: Vec<u8>,
    read_bytes: usize,
    fail_after: Option<usize>,
    complete_after_deadline: bool,
    deadline: Option<Instant>,
}
struct Memory {
    input: io::Cursor<Vec<u8>>,
    state: Arc<Mutex<State>>,
    chunk: usize,
}
impl io::Read for Memory {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let length = bytes.len().min(self.chunk);
        let count = self.input.read(bytes.split_at_mut(length).0)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("poisoned PF6000 fixture"))?;
        state.read_bytes += count;
        let delay = if state.complete_after_deadline
            && self.input.position()
                == u64::try_from(self.input.get_ref().len()).map_err(io::Error::other)?
        {
            Some(
                state
                    .deadline
                    .ok_or_else(|| io::Error::other("missing PF6000 fixture deadline"))?
                    .saturating_duration_since(Instant::now())
                    + Duration::from_millis(2),
            )
        } else {
            None
        };
        drop(state);
        if let Some(delay) = delay {
            thread::sleep(delay);
        }
        if count == 0 {
            Err(io::ErrorKind::TimedOut.into())
        } else {
            Ok(count)
        }
    }
}
impl Write for Memory {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("poisoned PF6000 fixture"))?;
        let length = state.fail_after.map_or(bytes.len(), |end| {
            end.saturating_sub(state.output.len()).min(bytes.len())
        });
        if length == 0 {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        state.output.extend_from_slice(bytes.split_at(length).0);
        Ok(length)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl net::Transport for Memory {
    fn set_deadline(&mut self, deadline: Instant) {
        self.state.lock().unwrap().deadline = Some(deadline);
    }
}
fn client(bytes: Vec<u8>, chunk: usize) -> (Client, Arc<Mutex<State>>) {
    let config = serde_json::from_value(json!({
        "profile":"pf6000_v2_8","address":"127.0.0.1:4545"
    }))
    .unwrap();
    let mut client = Client::new(config);
    let state = Arc::new(Mutex::new(State::default()));
    client.owner = Some(Box::new(Memory {
        input: io::Cursor::new(bytes),
        state: Arc::clone(&state),
        chunk,
    }));
    client.controller = Some(codec::startup(&literal("valid_startups", "startup_005")).unwrap());
    client.subscribed = true;
    client.initialized = true;
    (client, state)
}
fn kind(error: &anyhow::Error) -> ErrorKind {
    crate::types::classify(error).kind
}

#[test]
fn exact_reads_preserve_duplicates_and_never_ack_before_matching_checkpoint() {
    for chunk in [1, 419, usize::MAX] {
        let bytes = [
            event(),
            event(),
            literal("control_frames", "accepted_unsubscribe_one"),
        ]
        .concat();
        let (mut client, state) = client(bytes, chunk);
        let first = client.read(Duration::from_secs(2)).unwrap();
        let [first] = first.as_slice() else {
            panic!("one event")
        };
        assert_eq!(first.value, corpus()["valid_results"][0]["expected_value"]);
        assert!(first.source_time.is_none());
        let observed = client.observed_ns().unwrap();
        assert_eq!(state.lock().unwrap().read_bytes, 420);
        assert_eq!(state.lock().unwrap().output, Vec::<u8>::new());
        let deadline = client.checkpoint_deadline().unwrap();
        client.checkpointed().unwrap();
        assert_eq!(
            state.lock().unwrap().output,
            literal("outbound_frames", "result_ack")
        );
        let second = client.read(Duration::from_secs(2)).unwrap();
        assert_eq!(second.len(), 1);
        assert_eq!(second.first().unwrap().value, first.value);
        assert!(client.observed_ns().unwrap() >= observed);
        assert_eq!(state.lock().unwrap().read_bytes, 840);
        assert!(client.checkpoint_deadline().unwrap() >= deadline);
        client.checkpointed().unwrap();
        client.finish(Duration::from_secs(1)).unwrap();
        assert!(client.ready());
        assert_eq!(
            state.lock().unwrap().output,
            [
                literal("outbound_frames", "result_ack"),
                literal("outbound_frames", "result_ack"),
                literal("outbound_frames", "unsubscribe")
            ]
            .concat()
        );
        assert!(client.owner.is_none());
    }
}

#[test]
fn a_missing_stale_or_failed_checkpoint_retires_without_another_result_read() {
    let (mut source, state) = client([event(), event()].concat(), usize::MAX);
    source.read(Duration::from_secs(1)).unwrap();
    assert_eq!(
        kind(&source.read(Duration::from_secs(1)).unwrap_err()),
        ErrorKind::Incomplete
    );
    assert_eq!(state.lock().unwrap().read_bytes, 420);
    assert_eq!(state.lock().unwrap().output, Vec::<u8>::new());
    assert!(source.owner.is_none());
    for partial in [false, true] {
        let (mut source, state) = client(event(), usize::MAX);
        source.read(Duration::from_secs(1)).unwrap();
        if partial {
            state.lock().unwrap().fail_after = Some(7);
        } else {
            source.pending = Some(Instant::now());
        }
        let error = source.checkpointed().unwrap_err();
        assert_eq!(
            kind(&error),
            if partial {
                ErrorKind::Disconnected
            } else {
                ErrorKind::Timeout
            }
        );
        assert_eq!(
            state.lock().unwrap().output.len(),
            if partial { 7 } else { 0 }
        );
        assert!(source.owner.is_none());
        source.finish(Duration::from_secs(1)).unwrap();
        assert_eq!(
            state.lock().unwrap().output.len(),
            if partial { 7 } else { 0 }
        );
    }
}

#[test]
fn cutoff_is_exclusive_and_late_frames_have_raw_clock_evidence_without_ack() {
    let (mut source, state) = client(event(), usize::MAX);
    source.set_acquisition_end(Some(1000));
    assert!(source.within_cutoff(999));
    assert!(!source.within_cutoff(1000));
    assert!(!source.within_cutoff(1001));
    source.set_acquisition_end(Some(0));
    let error = source.read(Duration::from_secs(1)).unwrap_err();
    assert_eq!(kind(&error), ErrorKind::Timeout);
    let details = crate::types::classify(&error).details.unwrap();
    assert_eq!(
        details["atlas_open_protocol_frame"]["raw_hex"],
        crate::types::hex(event())
    );
    assert!(
        details["atlas_open_protocol_frame"]["host_observed_ns"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert_eq!(state.lock().unwrap().output, Vec::<u8>::new());
    assert!(source.checkpoint_deadline().is_none());
    let (mut source, state) = client(event(), usize::MAX);
    source.read(Duration::from_secs(2)).unwrap();
    source.set_acquisition_end(Some(0));
    assert_eq!(
        kind(&source.checkpointed().unwrap_err()),
        ErrorKind::Timeout
    );
    assert_eq!(state.lock().unwrap().output, Vec::<u8>::new());
}

#[test]
fn a_complete_native_read_returning_after_deadline_keeps_its_raw_receive_evidence() {
    let (mut source, state) = client(event(), usize::MAX);
    state.lock().unwrap().complete_after_deadline = true;
    let error = source.read(Duration::from_millis(100)).unwrap_err();
    assert_eq!(kind(&error), ErrorKind::Timeout);
    let details = crate::types::classify(&error).details.unwrap();
    assert_eq!(
        details["atlas_open_protocol_frame"]["raw_hex"],
        crate::types::hex(event())
    );
    assert!(
        details["atlas_open_protocol_frame"]["host_observed_ns"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert_eq!(state.lock().unwrap().output, Vec::<u8>::new());
    assert!(source.owner.is_none());
}

#[test]
fn keepalive_interleave_retains_original_deadline_and_rejects_unsolicited_replies() {
    let ping = literal("control_frames", "keepalive_reply_one");
    let (mut source, state) = client([event(), ping.clone()].concat(), usize::MAX);
    source.last_activity = Instant::now().checked_sub(Duration::from_secs(6)).unwrap();
    source.read(Duration::from_secs(2)).unwrap();
    let original = source.keepalive_deadline.unwrap();
    assert_eq!(source.checkpoint_deadline(), Some(original));
    assert_eq!(
        state.lock().unwrap().output,
        literal("outbound_frames", "keepalive")
    );
    source.checkpointed().unwrap();
    assert_idle(&source.read(Duration::from_secs(20)).unwrap());
    assert!(source.keepalive_deadline.is_none());
    assert_eq!(
        state.lock().unwrap().output,
        [
            literal("outbound_frames", "keepalive"),
            literal("outbound_frames", "result_ack")
        ]
        .concat()
    );
    let (mut source, state) = client([ping.clone(), ping].concat(), usize::MAX);
    source.keepalive_deadline = Some(Instant::now() + Duration::from_secs(1));
    let error = source.read(Duration::from_secs(20)).unwrap_err();
    assert_eq!(kind(&error), ErrorKind::Incomplete);
    assert!(format!("{error:#}").contains("unsolicited"));
    assert_eq!(state.lock().unwrap().output, Vec::<u8>::new());
    let mut wrong = literal("control_frames", "accepted_subscribe_one");
    assert_eq!(wrong.len(), 25);
    wrong[20..24].copy_from_slice(b"9999");
    let (mut source, state) = client(wrong, usize::MAX);
    source.keepalive_deadline = Some(Instant::now() + Duration::from_secs(1));
    assert_eq!(
        kind(&source.finish(Duration::from_secs(2)).unwrap_err()),
        ErrorKind::Incomplete
    );
    assert_eq!(state.lock().unwrap().output, Vec::<u8>::new());
    let (mut source, state) = client(event(), usize::MAX);
    source.keepalive_deadline = Some(Instant::now());
    assert_eq!(
        kind(&source.read(Duration::from_secs(20)).unwrap_err()),
        ErrorKind::Timeout
    );
    assert_eq!(state.lock().unwrap().read_bytes, 0);
    let (mut source, state) = client(Vec::new(), usize::MAX);
    source.last_activity = Instant::now().checked_sub(Duration::from_secs(6)).unwrap();
    assert_idle(
        &source
            .read_until(
                Instant::now() + Duration::from_secs(1),
                Duration::from_secs(2),
            )
            .unwrap(),
    );
    assert_eq!(state.lock().unwrap().output, Vec::<u8>::new());
    assert!(source.keepalive_deadline.is_none());
    source.cutoff = Some(crate::host::monotonic_ns().unwrap() + 5_000_000);
    assert_idle(&source.read(Duration::from_secs(1)).unwrap());
    assert_eq!(state.lock().unwrap().output, Vec::<u8>::new());
}

#[test]
fn finish_resolves_an_outstanding_keepalive_before_sending_unsubscribe() {
    let (mut source, state) = client(
        [
            literal("control_frames", "keepalive_reply_one"),
            literal("control_frames", "accepted_unsubscribe_one"),
        ]
        .concat(),
        usize::MAX,
    );
    source.keepalive_deadline = Some(Instant::now() + Duration::from_secs(1));
    source.finish(Duration::from_secs(2)).unwrap();
    assert_eq!(
        state.lock().unwrap().output,
        literal("outbound_frames", "unsubscribe")
    );
    let (mut source, state) = client(Vec::new(), usize::MAX);
    source.keepalive_deadline = Some(Instant::now());
    assert_eq!(
        kind(&source.finish(Duration::from_secs(2)).unwrap_err()),
        ErrorKind::Timeout
    );
    assert_eq!(state.lock().unwrap().output, Vec::<u8>::new());
}

#[test]
fn idle_partial_controls_and_shutdown_events_keep_distinct_failure_boundaries() {
    let (mut source, state) = client(Vec::new(), 1);
    assert_idle(&source.read(Duration::from_secs(1)).unwrap());
    assert!(source.ready());
    assert_idle(&source.read(Duration::ZERO).unwrap());
    assert_eq!(state.lock().unwrap().output, Vec::<u8>::new());
    let (mut source, state) = client(b"041".to_vec(), 1);
    assert_eq!(
        kind(&source.read(Duration::from_secs(1)).unwrap_err()),
        ErrorKind::Timeout
    );
    assert_eq!(state.lock().unwrap().read_bytes, 3);
    assert!(source.owner.is_none());
    let (mut source, state) = client(event(), usize::MAX);
    let error = source.finish(Duration::from_secs(1)).unwrap_err();
    assert_eq!(kind(&error), ErrorKind::Incomplete);
    assert_eq!(
        state.lock().unwrap().output,
        literal("outbound_frames", "unsubscribe")
    );
    let details = crate::types::classify(&error).details.unwrap();
    assert_eq!(
        details["atlas_open_protocol_frame"]["raw_hex"],
        crate::types::hex(event())
    );
    assert!(source.owner.is_none());
    let (mut source, state) = client(event(), usize::MAX);
    source.read(Duration::from_secs(1)).unwrap();
    assert_eq!(
        kind(&source.finish(Duration::from_secs(1)).unwrap_err()),
        ErrorKind::Incomplete
    );
    assert_eq!(state.lock().unwrap().output, Vec::<u8>::new());
}

#[test]
fn frame_fill_does_not_renew_deadlines_on_interrupts_or_treat_partial_timeout_as_idle() {
    let mut bytes = [0; 4];
    let deadline = Instant::now();
    let mut called = false;
    assert_eq!(
        kind(
            &fill(
                |_| {
                    called = true;
                    Err(io::ErrorKind::Interrupted.into())
                },
                &mut bytes,
                deadline,
                false
            )
            .unwrap_err()
        ),
        ErrorKind::Timeout
    );
    assert!(!called);
    let mut calls = 0;
    let error = fill(
        |target| {
            calls += 1;
            if calls == 1 {
                *target.first_mut().unwrap() = b'0';
                Ok(1)
            } else {
                Err(io::ErrorKind::TimedOut.into())
            }
        },
        &mut bytes,
        Instant::now() + Duration::from_secs(1),
        true,
    )
    .unwrap_err();
    assert_eq!(kind(&error), ErrorKind::Timeout);
    assert_eq!(calls, 2);
}
