use super::*;
use crate::transport::gige::bootstrap::Datagram;
use std::{cell::Cell, collections::VecDeque, io, rc::Rc};

// Independently declared fields: flag01, opcode0100, payload000c, ID3579,
// device12345678, group89abcdef, mask00010002. No product decoder oracle.
const ACTION_LITERAL: &str = "42010100000c35791234567889abcdef00010002";
const SUCCESS: [u8; 8] = [0, 0, 1, 1, 0, 0, 0x35, 0x79];

fn request() -> Request {
    Request {
        mode: Mode::Acknowledged,
        interface: Ipv4Addr::new(192, 168, 50, 1),
        device_key: 0x1234_5678,
        group_key: 0x89ab_cdef,
        group_mask: 0x0001_0002,
        action_time_ticks: None,
        expected: vec![
            Ipv4Addr::new(192, 168, 50, 10),
            Ipv4Addr::new(192, 168, 50, 11),
        ],
        timeout: Duration::from_secs(1),
    }
}

fn interface() -> Interface {
    Interface {
        name: "fixture".into(),
        index: 7,
        address: request().interface,
        netmask: Ipv4Addr::new(255, 255, 255, 0),
    }
}

struct Event {
    bytes: Vec<u8>,
    source: SocketAddrV4,
    selected: bool,
    advance: Duration,
}

fn reply(last: u8, bytes: &[u8]) -> Event {
    Event {
        bytes: bytes.to_vec(),
        source: SocketAddrV4::new(Ipv4Addr::new(192, 168, 50, last), 3956),
        selected: true,
        advance: Duration::ZERO,
    }
}

struct Script {
    clock: Rc<Cell<Instant>>,
    deadline: Instant,
    cancelled: Rc<Cell<bool>>,
    events: VecDeque<Event>,
    sent: Vec<Vec<u8>>,
    receive_calls: usize,
    send_error: Option<io::ErrorKind>,
    short_send: bool,
    expire_send: bool,
    cancel_send: bool,
}

impl Script {
    fn new(events: impl IntoIterator<Item = Event>) -> Self {
        let start = Instant::now();
        Self {
            clock: Rc::new(Cell::new(start)),
            deadline: start + Duration::from_secs(1),
            cancelled: Rc::new(Cell::new(false)),
            events: events.into_iter().collect(),
            sent: Vec::new(),
            receive_calls: 0,
            send_error: None,
            short_send: false,
            expire_send: false,
            cancel_send: false,
        }
    }
}

impl Channel for Script {
    fn send(&mut self, packet: &[u8]) -> io::Result<usize> {
        self.sent.push(packet.to_vec());
        if self.expire_send {
            self.clock.set(self.deadline);
        }
        if self.cancel_send {
            self.cancelled.set(true);
        }
        if let Some(kind) = self.send_error {
            return Err(io::Error::new(kind, "scripted send error"));
        }
        Ok(if self.short_send {
            packet.len() - 1
        } else {
            packet.len()
        })
    }

    fn receive(&mut self, buffer: &mut [u8]) -> io::Result<Option<Datagram>> {
        self.receive_calls += 1;
        let Some(event) = self.events.pop_front() else {
            self.clock.set(self.deadline);
            return Ok(None);
        };
        self.clock.set(self.clock.get() + event.advance);
        let count = event.bytes.len().min(buffer.len());
        buffer[..count].copy_from_slice(&event.bytes[..count]);
        Ok(Some(Datagram {
            bytes: event.bytes.len(),
            source: event.source,
            selected_interface: event.selected,
        }))
    }
}

fn evaluate(request: &Request, script: &mut Script) -> Result<Value> {
    request.validate()?;
    request.validate_interface(&interface())?;
    let clock = script.clock.clone();
    let cancelled = script.cancelled.clone();
    let now = || clock.get();
    let interrupted = || cancelled.get();
    let budget = Budget {
        deadline: script.deadline,
        interrupted: &interrupted,
        now: &now,
    };
    let mut progress = Progress::default();
    let attempt = exchange(request, script, budget, 0x3579, &mut progress, || Ok(()));
    result(request, &interface(), 0x3579, &progress, attempt, budget)
}

fn failed(value: Result<Value>, kind: ErrorKind) -> Value {
    let error = value.unwrap_err();
    let classified = crate::types::classify(&error);
    assert_eq!(classified.kind, kind, "{error:#}");
    classified.details.unwrap()
}

#[test]
fn literal_modes_one_send_and_strict_peer_collection() {
    let mut wrong_id = SUCCESS;
    wrong_id[7] = 0x78;
    let mut wrong_opcode = SUCCESS;
    wrong_opcode[3] = 0;
    let mut wrong_port = reply(10, &SUCCESS);
    wrong_port.source.set_port(5000);
    let mut wrong_interface = reply(10, &SUCCESS);
    wrong_interface.selected = false;
    let mut script = Script::new([
        reply(12, &SUCCESS),
        wrong_port,
        wrong_interface,
        reply(10, &wrong_id),
        reply(10, &wrong_opcode),
        reply(10, &SUCCESS),
        reply(10, &SUCCESS),
        reply(11, &SUCCESS),
    ]);
    let value = evaluate(&request(), &mut script).unwrap();
    assert_eq!(script.sent.len(), 1);
    assert_eq!(crate::types::hex(&script.sent[0]), ACTION_LITERAL);
    assert_eq!(value["outcome"], "acknowledged");
    assert_eq!(value["received_datagrams"], 8);
    assert_eq!(value["ignored_datagrams"], 5);
    assert_eq!(value["duplicate_datagrams"], 1);
    assert_eq!(value["responses"].as_array().unwrap().len(), 2);
    assert_eq!(value["missing"], json!([]));
    assert_eq!(value["frame_arrival"], "not_verified");
    assert_eq!(value["synchronization"], "not_verified");
    assert_eq!(value["execution"], "not_verified");
    assert_eq!(value["action"], "immediate");
    assert_eq!(value["bytes"], 20);
    assert!(value.get("action_time_ticks").is_none());
    let mut request = request();
    request.mode = Mode::SentOnly;
    request.expected.clear();
    let mut script = Script::new([reply(10, &SUCCESS)]);
    let value = evaluate(&request, &mut script).unwrap();
    assert_eq!(
        crate::types::hex(&script.sent[0]),
        "42000100000c35791234567889abcdef00010002"
    );
    assert_eq!(script.sent.len(), 1);
    assert_eq!(script.receive_calls, 0);
    assert_eq!(value["verification"], "not_requested");
    assert_eq!(value["outcome"], "sent_without_ack_confirmation");
}

#[test]
fn scheduled_literals_preserve_zero_and_all_u64_bits_in_both_modes() {
    // Hand-declared complete wire images: 0x80 selects the scheduled body,
    // 0x01 requests an ACK, and the final eight bytes are the caller's BE ticks.
    // Zero is still scheduled; no host/device-clock comparison is performed.
    for (ticks, acknowledged, sent_only) in [
        (
            0,
            "42810100001435791234567889abcdef000100020000000000000000",
            "42800100001435791234567889abcdef000100020000000000000000",
        ),
        (
            0x0102_0304_0506_0708,
            "42810100001435791234567889abcdef000100020102030405060708",
            "42800100001435791234567889abcdef000100020102030405060708",
        ),
        (
            9_007_199_254_740_993,
            "42810100001435791234567889abcdef000100020020000000000001",
            "42800100001435791234567889abcdef000100020020000000000001",
        ),
        (
            u64::MAX,
            "42810100001435791234567889abcdef00010002ffffffffffffffff",
            "42800100001435791234567889abcdef00010002ffffffffffffffff",
        ),
    ] {
        for mode in [Mode::Acknowledged, Mode::SentOnly] {
            let mut request = request();
            request.action_time_ticks = Some(ticks);
            request.mode = mode;
            if mode == Mode::SentOnly {
                request.expected.clear();
            }
            let mut script = Script::new([reply(10, &SUCCESS), reply(11, &SUCCESS)]);
            let value = evaluate(&request, &mut script).unwrap();
            assert_eq!(script.sent.len(), 1);
            assert_eq!(script.sent[0].len(), 28);
            assert_eq!(
                crate::types::hex(&script.sent[0]),
                if mode == Mode::Acknowledged {
                    acknowledged
                } else {
                    sent_only
                }
            );
            assert_eq!(value["action"], "scheduled");
            assert_eq!(value["action_time_ticks"].as_u64(), Some(ticks));
            assert_eq!(value["bytes"], 28);
            assert_eq!(value["mutation_datagrams"], 1);
            assert_eq!(value["execution"], "not_verified");
            assert_eq!(value["frame_arrival"], "not_verified");
            assert_eq!(value["synchronization"], "not_verified");
            assert_eq!(
                script.receive_calls,
                if mode == Mode::Acknowledged { 2 } else { 0 }
            );
        }
    }
}

#[test]
fn scheduled_remote_statuses_keep_raw_target_and_partial_acknowledgements() {
    for (status, name, bytes) in [
        (
            0x8013,
            "no_reference_time",
            [0x80, 0x13, 1, 1, 0, 0, 0x35, 0x79],
        ),
        (0x8015, "overflow", [0x80, 0x15, 1, 1, 0, 0, 0x35, 0x79]),
        (0x8016, "action_late", [0x80, 0x16, 1, 1, 0, 0, 0x35, 0x79]),
        (0xc123, "unknown", [0xc1, 0x23, 1, 1, 0, 0, 0x35, 0x79]),
    ] {
        let mut request = request();
        request.action_time_ticks = Some(u64::MAX);
        let mut script = Script::new([reply(10, &SUCCESS), reply(11, &bytes)]);
        let details = failed(evaluate(&request, &mut script), ErrorKind::OperationFailed);
        assert_eq!(details["action_failure"], "remote_status");
        let value = &details["action"];
        assert_eq!(value["action"], "scheduled");
        assert_eq!(value["action_time_ticks"].as_u64(), Some(u64::MAX));
        assert_eq!(value["responses"].as_array().unwrap().len(), 2);
        assert_eq!(value["responses"][0]["status_raw"], 0);
        assert_eq!(value["responses"][1]["status_raw"], status);
        assert_eq!(value["responses"][1]["status"], name);
        assert_eq!(value["bytes"], 28);
        assert_eq!(value["outcome"], "unconfirmed");
        assert_eq!(value["action_may_have_been_issued"], true);
        assert_eq!(value["execution"], "not_verified");
        assert_eq!(script.sent.len(), 1);
        assert_eq!(script.receive_calls, 2);
    }
}

#[test]
fn scheduled_target_does_not_extend_deadline_or_allow_retry() {
    let mut request = request();
    let expected = request.expected.clone();
    request.action_time_ticks = Some(u64::MAX);
    let mut script = Script::new([]);
    script.short_send = true;
    let details = failed(evaluate(&request, &mut script), ErrorKind::Incomplete);
    assert_eq!(details["action_failure"], "short_send");
    assert_eq!(script.sent[0].len(), 28);
    assert_eq!(script.sent.len(), 1);
    assert_eq!(script.receive_calls, 0);
    assert_eq!(details["action"]["mutation_datagrams"], 0);
    assert_eq!(details["action"]["bytes"], 0);
    assert_eq!(details["action"]["action_may_have_been_issued"], true);

    for mode in [Mode::Acknowledged, Mode::SentOnly] {
        request.mode = mode;
        if mode == Mode::SentOnly {
            request.expected.clear();
        }
        let mut script = Script::new([]);
        script.expire_send = true;
        let details = failed(evaluate(&request, &mut script), ErrorKind::Timeout);
        assert_eq!(details["action"]["mutation_datagrams"], 1);
        assert_eq!(details["action"]["bytes"], 28);
        assert_eq!(details["action"]["outcome"], "unconfirmed");
        assert_eq!(script.sent.len(), 1);
        assert_eq!(script.receive_calls, 0);

        let mut script = Script::new([]);
        script.cancel_send = true;
        let details = failed(evaluate(&request, &mut script), ErrorKind::Interrupted);
        assert_eq!(details["action"]["mutation_send_attempted"], true);
        assert_eq!(script.sent.len(), 1);
        assert_eq!(script.receive_calls, 0);
    }

    request.mode = Mode::Acknowledged;
    request.expected = expected;
    let mut late = reply(11, &SUCCESS);
    late.advance = Duration::from_secs(1);
    let mut script = Script::new([reply(10, &SUCCESS), late]);
    let details = failed(evaluate(&request, &mut script), ErrorKind::Timeout);
    assert_eq!(details["action"]["responses"].as_array().unwrap().len(), 1);
    assert_eq!(
        details["action"]["action_time_ticks"].as_u64(),
        Some(u64::MAX)
    );
    assert_eq!(script.sent.len(), 1);
    assert_eq!(script.receive_calls, 2);

    for cancel in [false, true] {
        let mut script = Script::new([]);
        if cancel {
            script.cancelled.set(true);
        } else {
            script.clock.set(script.deadline);
        }
        let details = failed(
            evaluate(&request, &mut script),
            if cancel {
                ErrorKind::Interrupted
            } else {
                ErrorKind::Timeout
            },
        );
        assert_eq!(details["action"]["mutation_send_attempted"], false);
        assert_eq!(script.sent, [] as [Vec<u8>; 0]);
        assert_eq!(script.receive_calls, 0);
    }
}

#[test]
fn malformed_remote_conflicting_and_missing_replies_retain_partial_results() {
    let mut with_body = SUCCESS.to_vec();
    with_body.push(0);
    let mut declared_body = SUCCESS;
    declared_body[5] = 1;
    let mut consistent_body = declared_body.to_vec();
    consistent_body.push(0);
    for bytes in [with_body, declared_body.to_vec(), consistent_body] {
        let mut script = Script::new([reply(10, &SUCCESS), reply(11, &bytes)]);
        let details = failed(evaluate(&request(), &mut script), ErrorKind::Incomplete);
        assert_eq!(details["action_failure"], "malformed_reply");
        assert_eq!(details["action"]["responses"].as_array().unwrap().len(), 1);
        assert_eq!(details["action"]["action_may_have_been_issued"], true);
        assert_eq!(script.sent.len(), 1);
    }
    let unknown = [0xc1, 0x23, 1, 1, 0, 0, 0x35, 0x79];
    let mut script = Script::new([reply(10, &unknown)]);
    let details = failed(
        evaluate(&request(), &mut script),
        ErrorKind::OperationFailed,
    );
    assert_eq!(details["action_failure"], "remote_status");
    assert_eq!(details["action"]["responses"][0]["status_raw"], 0xc123);
    assert_eq!(details["action"]["responses"][0]["status"], "unknown");
    assert_eq!(script.sent.len(), 1);
    let mut script = Script::new([reply(10, &SUCCESS), reply(10, &unknown)]);
    let details = failed(
        evaluate(&request(), &mut script),
        ErrorKind::CameraAmbiguous,
    );
    assert_eq!(details["action_failure"], "response_conflict");
    assert_eq!(details["conflicting_status_raw"], 0xc123);
    let mut script = Script::new([reply(10, &SUCCESS), reply(11, &SUCCESS[..7])]);
    let details = failed(evaluate(&request(), &mut script), ErrorKind::Timeout);
    assert_eq!(details["action"]["responses"].as_array().unwrap().len(), 1);
    assert_eq!(details["action"]["missing"], json!(["192.168.50.11"]));
    assert_eq!(details["action"]["outcome"], "unconfirmed");
    assert_eq!(script.sent.len(), 1);
}

#[test]
fn every_send_result_is_one_attempt_and_late_or_cancelled_success_is_refused() {
    for kind in [
        io::ErrorKind::WouldBlock,
        io::ErrorKind::Interrupted,
        io::ErrorKind::Other,
    ] {
        let mut script = Script::new([]);
        script.send_error = Some(kind);
        let error = evaluate(&request(), &mut script).unwrap_err();
        let details = crate::types::classify(&error).details.unwrap();
        assert_eq!(details["action"]["mutation_send_attempted"], true);
        assert_eq!(details["action"]["mutation_datagrams"], 0);
        assert_eq!(details["action"]["action_may_have_been_issued"], true);
        assert_eq!(script.sent.len(), 1);
        assert_eq!(script.receive_calls, 0);
    }
    let mut script = Script::new([]);
    script.short_send = true;
    let details = failed(evaluate(&request(), &mut script), ErrorKind::Incomplete);
    assert_eq!(details["action_failure"], "short_send");
    assert_eq!(script.sent.len(), 1);
    let mut script = Script::new([]);
    script.expire_send = true;
    let details = failed(evaluate(&request(), &mut script), ErrorKind::Timeout);
    assert_eq!(details["action"]["mutation_datagrams"], 1);
    assert_eq!(details["action"]["outcome"], "unconfirmed");
    let mut script = Script::new([]);
    script.cancel_send = true;
    let details = failed(evaluate(&request(), &mut script), ErrorKind::Interrupted);
    assert_eq!(details["action"]["mutation_send_attempted"], true);
    let mut late = reply(11, &SUCCESS);
    late.advance = Duration::from_secs(1);
    let mut script = Script::new([reply(10, &SUCCESS), late]);
    let details = failed(evaluate(&request(), &mut script), ErrorKind::Timeout);
    assert_eq!(details["action"]["received_datagrams"], 2);
    assert_eq!(details["action"]["responses"].as_array().unwrap().len(), 1);
    assert_eq!(script.sent.len(), 1);
}

#[test]
fn pre_send_expiry_and_interface_revalidation_make_no_attempt() {
    let mut script = Script::new([]);
    let clock = script.clock.clone();
    let deadline = script.deadline;
    let now = || clock.get();
    let start = clock.get();
    let interrupted = || false;
    let budget = Budget {
        deadline,
        interrupted: &interrupted,
        now: &now,
    };
    let mut progress = Progress::default();
    let error = exchange(
        &request(),
        &mut script,
        budget,
        0x3579,
        &mut progress,
        || {
            clock.set(deadline);
            Ok(())
        },
    )
    .unwrap_err();
    assert_eq!(crate::types::classify(&error).kind, ErrorKind::Timeout);
    assert!(!progress.attempted);
    assert_eq!(script.sent, [] as [Vec<u8>; 0]);
    clock.set(start);
    let error = exchange(
        &request(),
        &mut script,
        budget,
        0x3579,
        &mut progress,
        || {
            Err(Categorized::error(
                ErrorKind::Unavailable,
                "interface changed",
            ))
        },
    )
    .unwrap_err();
    assert_eq!(crate::types::classify(&error).kind, ErrorKind::Unavailable);
    assert!(!progress.attempted);
    assert_eq!(script.sent, [] as [Vec<u8>; 0]);
    let mut script = Script::new([]);
    script.cancelled.set(true);
    let details = failed(evaluate(&request(), &mut script), ErrorKind::Interrupted);
    assert_eq!(details["action"]["mutation_send_attempted"], false);
    assert_eq!(script.sent, [] as [Vec<u8>; 0]);
}

#[test]
fn completed_ack_set_is_not_published_after_the_original_deadline() {
    let now = Instant::now();
    let budget = Budget {
        deadline: now,
        interrupted: &|| false,
        now: &|| now,
    };
    let progress = Progress {
        attempted: true,
        sent: true,
        completed: true,
        replies: vec![Reply {
            source: SocketAddrV4::new(request().expected[0], 3956),
            status_raw: 0,
            status: "success",
        }],
        ..Progress::default()
    };
    let details = failed(
        result(&request(), &interface(), 0x3579, &progress, Ok(()), budget),
        ErrorKind::Timeout,
    );
    assert_eq!(details["action"]["outcome"], "unconfirmed");
    assert_eq!(details["action"]["verification"], "unconfirmed");
    assert_eq!(details["action"]["responses"].as_array().unwrap().len(), 1);
}

#[test]
fn foreign_flood_is_bounded_without_another_send_or_receive() {
    let mut script =
        Script::new(std::iter::repeat_with(|| reply(12, &SUCCESS)).take(MAX_DATAGRAMS));
    let details = failed(evaluate(&request(), &mut script), ErrorKind::Incomplete);
    assert_eq!(details["action_failure"], "traffic_limit");
    assert_eq!(details["action"]["received_datagrams"], MAX_DATAGRAMS);
    assert_eq!(details["action"]["ignored_datagrams"], MAX_DATAGRAMS);
    assert_eq!(script.receive_calls, MAX_DATAGRAMS);
    assert_eq!(script.sent.len(), 1);
}

#[test]
fn invalid_modes_keys_peers_subnets_and_durations_are_pre_io() {
    let mut cases = Vec::new();
    for timeout in [Duration::ZERO, Duration::from_secs(31), Duration::MAX] {
        let mut r = request();
        r.timeout = timeout;
        cases.push(r);
    }
    let mut r = request();
    r.group_mask = 0;
    cases.push(r);
    let mut r = request();
    r.expected.clear();
    cases.push(r);
    let mut r = request();
    r.expected = vec![Ipv4Addr::new(192, 168, 50, 10); 33];
    cases.push(r);
    let mut r = request();
    r.mode = Mode::SentOnly;
    cases.push(r);
    for peer in [
        Ipv4Addr::UNSPECIFIED,
        Ipv4Addr::BROADCAST,
        Ipv4Addr::new(224, 0, 0, 1),
        request().interface,
        Ipv4Addr::new(192, 168, 50, 10),
    ] {
        let mut r = request();
        r.expected.push(peer);
        cases.push(r);
    }
    for r in cases {
        let mut script = Script::new([]);
        evaluate(&r, &mut script).unwrap_err();
        assert_eq!(script.sent, [] as [Vec<u8>; 0]);
        assert_eq!(script.receive_calls, 0);
    }
    for peer in [
        Ipv4Addr::new(192, 168, 50, 0),
        Ipv4Addr::new(192, 168, 50, 255),
        Ipv4Addr::new(192, 168, 51, 10),
    ] {
        let mut r = request();
        r.expected = vec![peer];
        let mut script = Script::new([]);
        evaluate(&r, &mut script).unwrap_err();
        assert_eq!(script.sent, [] as [Vec<u8>; 0]);
    }
    for mask in [
        Ipv4Addr::UNSPECIFIED,
        Ipv4Addr::BROADCAST,
        Ipv4Addr::new(255, 0, 255, 0),
        Ipv4Addr::new(255, 255, 255, 254),
    ] {
        let mut i = interface();
        i.netmask = mask;
        request().validate_interface(&i).unwrap_err();
    }
}

#[test]
fn actual_loopback_socket_uses_source_and_receive_interface_without_broadcast() -> Result<()> {
    use std::net::{IpAddr, SocketAddr, UdpSocket};
    struct Local {
        socket: bootstrap::Socket,
        peer: SocketAddrV4,
        sends: usize,
    }
    impl Channel for Local {
        fn send(&mut self, packet: &[u8]) -> io::Result<usize> {
            self.sends += 1;
            self.socket.test_send_to(packet, self.peer)
        }
        fn receive(&mut self, buffer: &mut [u8]) -> io::Result<Option<Datagram>> {
            self.socket.receive(buffer)
        }
        fn test_source_port(&self) -> u16 {
            self.peer.port()
        }
    }
    let selected = if_addrs::get_if_addrs()?
        .into_iter()
        .find(|i| i.ip() == IpAddr::V4(Ipv4Addr::LOCALHOST))
        .context("missing loopback interface")?;
    let if_addrs::IfAddr::V4(address) = selected.addr else {
        anyhow::bail!("loopback is not IPv4");
    };
    let interface = Interface {
        name: selected.name,
        index: selected.index.context("loopback index missing")?,
        address: address.ip,
        netmask: address.netmask,
    };
    let peer = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))?;
    peer.set_read_timeout(Some(Duration::from_secs(2)))?;
    let SocketAddr::V4(endpoint) = peer.local_addr()? else {
        anyhow::bail!("peer is not IPv4");
    };
    let socket = bootstrap::Socket::open(&interface)?;
    let server = std::thread::spawn(move || -> Result<()> {
        let mut buffer = [0; 64];
        let (bytes, source) = peer.recv_from(&mut buffer)?;
        anyhow::ensure!(
            source.ip() == IpAddr::V4(Ipv4Addr::LOCALHOST),
            "wrong actual source address"
        );
        anyhow::ensure!(
            crate::types::hex(&buffer[..bytes]) == ACTION_LITERAL,
            "wrong actual command bytes"
        );
        anyhow::ensure!(peer.send_to(&SUCCESS, source)? == 8, "short actual ACK");
        Ok(())
    });
    let mut channel = Local {
        socket,
        peer: endpoint,
        sends: 0,
    };
    let mut request = request();
    request.interface = Ipv4Addr::LOCALHOST;
    request.expected = vec![Ipv4Addr::LOCALHOST];
    request.timeout = Duration::from_secs(2);
    test_exchange(&request, &interface, &mut channel, 0x3579, &|| false).unwrap_err();
    assert_eq!(channel.sends, 0);
    let response = test_fixture_exchange(&request, &interface, &mut channel, 0x3579, &|| false);
    server
        .join()
        .map_err(|_| anyhow::anyhow!("loopback peer panicked"))??;
    let value = response?;
    assert_eq!(channel.sends, 1);
    assert_eq!(value["outcome"], "acknowledged");
    assert_eq!(value["responses"][0]["source"], endpoint.to_string());
    Ok(())
}
