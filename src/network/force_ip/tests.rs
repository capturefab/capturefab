use super::*;
use crate::transport::gige::bootstrap::Datagram;
use std::{cell::Cell, collections::VecDeque, io, rc::Rc};

const FORCE_LITERAL: &str = concat!(
    "42110004003835790000003053061a58",
    "000000000000000000000000c0a83214",
    "000000000000000000000000ffffff00",
    "000000000000000000000000c0a83201"
);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Apply,
    LostAck,
    AckOnly,
    WrongId,
    WrongInterface,
    WrongMac,
    Status,
    NoTarget,
    Collision,
    Changing,
    Flood,
    SendFailure,
    LateBefore,
    LateAfter,
}

struct Script {
    mode: Mode,
    request: Request,
    packets: Vec<Vec<u8>>,
    replies: VecDeque<(Vec<u8>, bool)>,
    mutations: usize,
    delayed: bool,
    cancel_after_mutation: Option<Rc<Cell<bool>>>,
}

impl Script {
    fn new(mode: Mode, request: Request) -> Self {
        Self {
            mode,
            request,
            packets: Vec::new(),
            replies: VecDeque::new(),
            mutations: 0,
            delayed: false,
            cancel_after_mutation: None,
        }
    }
}

fn word(packet: &[u8], offset: usize) -> io::Result<u16> {
    let bytes = packet
        .get(offset..offset + 2)
        .ok_or_else(|| io::Error::other("short fake command"))?;
    Ok(u16::from_be_bytes(
        bytes.try_into().map_err(io::Error::other)?,
    ))
}

fn reply(command: u16, id: u16, payload: &[u8]) -> Vec<u8> {
    let mut packet = vec![0; 8];
    packet[2..4].copy_from_slice(&command.to_be_bytes());
    packet[4..6].copy_from_slice(&u16::try_from(payload.len()).unwrap().to_be_bytes());
    packet[6..8].copy_from_slice(&id.to_be_bytes());
    packet.extend_from_slice(payload);
    packet
}

fn discovered(request: Request, id: u16, configuration: Configuration) -> Vec<u8> {
    let mut payload = [0; 0xf8];
    payload[10..16].copy_from_slice(&request.mac);
    payload[0x24..0x28].copy_from_slice(&configuration.address.octets());
    payload[0x34..0x38].copy_from_slice(&configuration.netmask.octets());
    payload[0x44..0x48].copy_from_slice(&configuration.gateway.octets());
    reply(DISCOVERY + 1, id, &payload)
}

const fn previous() -> Configuration {
    Configuration {
        address: Ipv4Addr::new(10, 20, 30, 40),
        netmask: Ipv4Addr::new(255, 255, 255, 0),
        gateway: Ipv4Addr::UNSPECIFIED,
    }
}

impl Channel for Script {
    fn send(&mut self, packet: &[u8]) -> io::Result<usize> {
        self.packets.push(packet.to_vec());
        let id = word(packet, 6)?;
        match word(packet, 2)? {
            FORCE_IP => {
                self.mutations += 1;
                if let Some(cancelled) = &self.cancel_after_mutation {
                    cancelled.set(true);
                }
                if self.mode == Mode::SendFailure {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "fake mutation send blocked",
                    ));
                }
                if self.mode != Mode::LostAck {
                    let mut ack = reply(FORCE_IP + 1, id, &[]);
                    if self.mode == Mode::Status {
                        ack[..2].copy_from_slice(&0x8006_u16.to_be_bytes());
                    }
                    self.replies.push_back((ack, true));
                }
            }
            DISCOVERY => {
                if self.mode == Mode::NoTarget {
                    return Ok(packet.len());
                }
                let post = self.mutations != 0;
                let configuration = if post && self.mode != Mode::AckOnly {
                    self.request.configuration
                } else {
                    previous()
                };
                let mut camera = self.request;
                if post && self.mode == Mode::WrongMac {
                    camera.mac[5] ^= 1;
                }
                let id = if post && self.mode == Mode::WrongId {
                    id.wrapping_add(17)
                } else {
                    id
                };
                let packet = discovered(camera, id, configuration);
                self.replies
                    .push_back((packet, !(post && self.mode == Mode::WrongInterface)));
                if !post && self.mode == Mode::Collision {
                    camera.mac[5] ^= 1;
                    self.replies
                        .push_back((discovered(camera, id, self.request.configuration), true));
                }
                if !post && self.mode == Mode::Changing {
                    self.replies
                        .push_back((discovered(camera, id, self.request.configuration), true));
                }
            }
            _ => return Err(io::Error::other("unexpected fake bootstrap command")),
        }
        Ok(packet.len())
    }

    fn receive(&mut self, buffer: &mut [u8]) -> io::Result<Option<Datagram>> {
        if !self.delayed
            && ((self.mode == Mode::LateBefore && self.mutations == 0)
                || (self.mode == Mode::LateAfter && self.mutations != 0))
        {
            self.delayed = true;
            std::thread::sleep(self.request.timeout + Duration::from_millis(10));
        }
        let next = if self.mode == Mode::Flood && self.mutations != 0 {
            Some((reply(FORCE_IP + 1, 999, &[]), true))
        } else {
            self.replies.pop_front()
        };
        let Some((packet, selected_interface)) = next else {
            return Ok(None);
        };
        let bytes = packet.len();
        buffer
            .get_mut(..bytes)
            .ok_or_else(|| io::Error::other("fake buffer too small"))?
            .copy_from_slice(&packet);
        Ok(Some(Datagram {
            bytes,
            source: SocketAddrV4::new(Ipv4Addr::new(192, 168, 50, 20), 3956),
            selected_interface,
        }))
    }
}

const fn request() -> Request {
    Request {
        mac: [0, 0x30, 0x53, 6, 0x1a, 0x58],
        interface: Ipv4Addr::new(192, 168, 50, 1),
        configuration: Configuration {
            address: Ipv4Addr::new(192, 168, 50, 20),
            netmask: Ipv4Addr::new(255, 255, 255, 0),
            gateway: Ipv4Addr::new(192, 168, 50, 1),
        },
        timeout: Duration::from_millis(300),
    }
}

fn exercise(mode: Mode) -> (Result<()>, Script, Progress) {
    let request = request();
    let mut script = Script::new(mode, request);
    let mut progress = Progress::default();
    let result = assign(
        request,
        &mut script,
        Budget::new(Instant::now() + request.timeout, &|| false),
        [0x3578, 0x3579, 0x357a],
        &mut progress,
        || Ok(()),
    );
    (result, script, progress)
}

#[test]
fn independent_force_ip_literal_and_request_ids_are_exact() {
    // PCCC's hand-authored literal follows pinned Wireshark offsets and viva's
    // emitter; it was frozen before the product encoder was inspected.
    assert_eq!(crate::types::hex(encode(request(), 0x3579)), FORCE_LITERAL);
    assert_eq!(
        command(DISCOVERY, 0x3578),
        [0x42, 0x11, 0, 2, 0, 0, 0x35, 0x78]
    );
    assert_eq!(parse_mac("00:30:53:06:1A:58").unwrap(), request().mac);
    assert_eq!(parse_mac("003053061a58").unwrap(), request().mac);
    for invalid in [
        "",
        "003053061a5g",
        "00:30:53:06:",
        "003053061a5:",
        "003053061a5",
        "003053061a5800",
        "00-30-53-06-1a-58",
        "000000000000",
        "013053061a58",
        "ffffffffffff",
    ] {
        assert_ne!(parse_mac(invalid).unwrap_err(), "");
    }
}

#[test]
fn one_mutation_needs_fresh_configuration_even_without_ack() {
    for mode in [Mode::Apply, Mode::LostAck] {
        let (result, script, progress) = exercise(mode);
        result.unwrap();
        assert_eq!(script.mutations, 1);
        assert_eq!(script.packets.iter().filter(|p| p.len() == 64).count(), 1);
        assert_eq!(progress.before.unwrap().configuration, previous());
        assert_eq!(
            progress.observed.unwrap().configuration,
            request().configuration
        );
        assert_eq!(progress.ack.is_some(), mode == Mode::Apply);
        assert!(progress.sent && progress.send_attempted);
    }
}

#[test]
fn ack_and_wrong_identity_or_interface_never_replace_fresh_proof() {
    for mode in [
        Mode::AckOnly,
        Mode::WrongId,
        Mode::WrongInterface,
        Mode::WrongMac,
    ] {
        let (result, script, progress) = exercise(mode);
        let error = result.unwrap_err();
        assert_ne!(error.to_string(), "");
        assert_eq!(script.mutations, 1);
        assert_eq!(progress.observed, None);
        assert!(progress.send_attempted && progress.sent);
    }
}

#[test]
fn preflight_collision_changing_identity_and_absence_send_no_assignment() {
    for mode in [
        Mode::NoTarget,
        Mode::Collision,
        Mode::Changing,
        Mode::LateBefore,
    ] {
        let (result, script, progress) = exercise(mode);
        assert_ne!(result.unwrap_err().to_string(), "");
        assert_eq!(script.mutations, 0);
        assert!(!progress.send_attempted);
    }
}

#[test]
fn status_flood_late_results_and_send_failure_never_retry_or_cleanup() {
    for mode in [
        Mode::Status,
        Mode::Flood,
        Mode::LateAfter,
        Mode::SendFailure,
    ] {
        let (result, script, progress) = exercise(mode);
        assert_ne!(result.unwrap_err().to_string(), "");
        assert_eq!(script.mutations, 1);
        assert!(progress.send_attempted);
        assert!(
            script
                .packets
                .iter()
                .all(|p| word(p, 2).unwrap() == DISCOVERY || word(p, 2).unwrap() == FORCE_IP)
        );
        if mode == Mode::SendFailure {
            assert!(!progress.sent);
        }
        if mode == Mode::Flood {
            assert_eq!(progress.received, MAX_DATAGRAMS + 1);
        }
    }
}

#[test]
fn pre_mutation_owner_recheck_and_expired_deadline_have_zero_mutations() {
    let request = request();
    let mut script = Script::new(Mode::Apply, request);
    let mut progress = Progress::default();
    let error = assign(
        request,
        &mut script,
        Budget::new(Instant::now() + request.timeout, &|| false),
        [1, 2, 3],
        &mut progress,
        || Err(Categorized::error(ErrorKind::CameraBusy, "owner appeared")),
    )
    .unwrap_err();
    assert_eq!(
        error.downcast_ref::<Categorized>().unwrap().kind,
        ErrorKind::CameraBusy
    );
    assert_eq!(script.mutations, 0);
    let mut script = Script::new(Mode::Apply, request);
    let error = assign(
        request,
        &mut script,
        Budget::new(Instant::now(), &|| false),
        [1, 2, 3],
        &mut Progress::default(),
        || Ok(()),
    )
    .unwrap_err();
    assert_eq!(
        error.downcast_ref::<Categorized>().unwrap().kind,
        ErrorKind::Timeout
    );
    assert_eq!(script.packets, Vec::<Vec<u8>>::new());
}

#[test]
fn cancellation_preserves_post_send_uncertainty_and_sends_no_cleanup() {
    let request = request();
    let cancelled = Rc::new(Cell::new(false));
    let interrupted = || cancelled.get();
    let mut script = Script::new(Mode::Apply, request);
    script.cancel_after_mutation = Some(Rc::clone(&cancelled));
    let mut progress = Progress::default();
    let error = assign(
        request,
        &mut script,
        Budget::new(Instant::now() + request.timeout, &interrupted),
        [1, 2, 3],
        &mut progress,
        || Ok(()),
    )
    .unwrap_err();
    assert_eq!(
        error.downcast_ref::<Categorized>().unwrap().kind,
        ErrorKind::Interrupted
    );
    assert!(progress.send_attempted && progress.sent);
    assert_eq!(script.mutations, 1);
    assert_eq!(script.packets.len(), 2);
    assert_eq!(progress.observed, None);

    cancelled.set(false);
    let mut script = Script::new(Mode::Apply, request);
    let mut progress = Progress::default();
    let error = assign(
        request,
        &mut script,
        Budget::new(Instant::now() + request.timeout, &interrupted),
        [1, 2, 3],
        &mut progress,
        || {
            cancelled.set(true);
            Ok(())
        },
    )
    .unwrap_err();
    assert_eq!(
        error.downcast_ref::<Categorized>().unwrap().kind,
        ErrorKind::Interrupted
    );
    assert!(!progress.send_attempted);
    assert_eq!(script.mutations, 0);
}

#[test]
fn exact_address_validation_precedes_socket_or_device_work() {
    request().validate().unwrap();
    for address in [
        Ipv4Addr::UNSPECIFIED,
        Ipv4Addr::BROADCAST,
        Ipv4Addr::LOCALHOST,
        Ipv4Addr::new(224, 0, 0, 1),
        Ipv4Addr::new(192, 168, 50, 0),
        Ipv4Addr::new(192, 168, 50, 255),
        Ipv4Addr::new(192, 168, 50, 1),
        Ipv4Addr::new(192, 168, 51, 2),
    ] {
        let mut bad = request();
        bad.configuration.address = address;
        assert_ne!(bad.validate().unwrap_err().to_string(), "");
    }
    for mask in [
        Ipv4Addr::UNSPECIFIED,
        Ipv4Addr::BROADCAST,
        Ipv4Addr::new(255, 0, 255, 0),
        Ipv4Addr::new(255, 255, 255, 254),
    ] {
        let mut bad = request();
        bad.configuration.netmask = mask;
        assert_ne!(bad.validate().unwrap_err().to_string(), "");
    }
    for gateway in [
        Ipv4Addr::new(192, 168, 50, 20),
        Ipv4Addr::new(192, 168, 51, 1),
        Ipv4Addr::BROADCAST,
    ] {
        let mut bad = request();
        bad.configuration.gateway = gateway;
        assert_ne!(bad.validate().unwrap_err().to_string(), "");
    }
}

#[test]
fn acknowledgement_extents_and_post_send_error_summary_are_explicit() {
    let source = SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 2), 3956);
    let mut malformed = discovered(request(), 10, previous());
    malformed.pop();
    assert!(
        observation(&malformed, source)
            .unwrap_err()
            .to_string()
            .contains("length")
    );
    let mut truncated = reply(DISCOVERY + 1, 10, &[0; 10]);
    assert!(
        observation(&truncated, source)
            .unwrap_err()
            .to_string()
            .contains("short")
    );
    truncated[4..6].copy_from_slice(&0_u16.to_be_bytes());
    assert!(
        payload(&truncated)
            .unwrap_err()
            .to_string()
            .contains("length")
    );
    let (_, _, progress) = exercise(Mode::AckOnly);
    let interface = Interface {
        name: "literal-nic".into(),
        index: 7,
        address: request().interface,
        netmask: request().configuration.netmask,
    };
    let result = summary(request(), &interface, &progress);
    assert_eq!(result["assignment_may_have_applied"], true);
    assert_eq!(result["mutation_datagrams"], 1);
    assert_eq!(result["verification"], "unconfirmed");
    assert!(result.get("observed").is_none());
    assert_eq!(result["reachability"], "not_probed");
}

#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    windows
))]
#[test]
fn actual_loopback_peer_checks_one_literal_assignment_and_fresh_configuration() -> Result<()> {
    use crate::transport::gige::bootstrap::Socket;
    use std::{
        net::{IpAddr, SocketAddr, UdpSocket},
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
    };

    struct PeerChannel {
        socket: Socket,
        peer: SocketAddrV4,
    }
    impl Channel for PeerChannel {
        fn send(&mut self, packet: &[u8]) -> io::Result<usize> {
            self.socket.test_send_to(packet, self.peer)
        }
        fn receive(&mut self, buffer: &mut [u8]) -> io::Result<Option<Datagram>> {
            self.socket.receive(buffer)
        }
        fn test_source_port(&self) -> u16 {
            self.peer.port()
        }
    }

    let mut request = request();
    request.timeout = Duration::from_secs(2);
    let selected = if_addrs::get_if_addrs()?
        .into_iter()
        .find(|i| i.ip() == IpAddr::V4(Ipv4Addr::LOCALHOST))
        .context("IPv4 loopback interface missing")?;
    let if_addrs::IfAddr::V4(address) = selected.addr else {
        anyhow::bail!("loopback source is not IPv4");
    };
    let interface = Interface {
        name: selected.name,
        index: selected.index.context("loopback index missing")?,
        address: Ipv4Addr::LOCALHOST,
        netmask: address.netmask,
    };
    let socket = Socket::open(&interface)?;
    let peer = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))?;
    peer.set_read_timeout(Some(Duration::from_millis(50)))?;
    let SocketAddr::V4(endpoint) = peer.local_addr()? else {
        anyhow::bail!("fake peer is not IPv4");
    };
    let stop = Arc::new(AtomicBool::new(false));
    let shutdown = Arc::clone(&stop);
    let server = std::thread::spawn(move || -> Result<usize> {
        let until = Instant::now() + Duration::from_secs(4);
        let mut bytes = [0; 512];
        let mut mutations = 0;
        let mut configuration = previous();
        while Instant::now() < until && !shutdown.load(Ordering::Acquire) {
            let (count, source) = match peer.recv_from(&mut bytes) {
                Ok(received) => received,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            ensure!(source.ip().is_loopback(), "fixture escaped loopback");
            let packet = &bytes[..count];
            match word(packet, 2)? {
                FORCE_IP => {
                    ensure!(
                        crate::types::hex(packet) == FORCE_LITERAL,
                        "fake peer rejected a nonliteral assignment"
                    );
                    mutations += 1;
                    ensure!(mutations == 1, "assignment was retried");
                    configuration = request.configuration;
                    ensure!(
                        peer.send_to(&[0, 0, 0, 5, 0, 0, 0x35, 0x79], source)? == 8,
                        "short fake ACK"
                    );
                }
                DISCOVERY => {
                    ensure!(
                        count == 8 && packet[..2] == [0x42, 0x11],
                        "invalid fake discovery query"
                    );
                    let id = word(packet, 6)?;
                    ensure!(
                        id == if mutations == 0 { 0x3578 } else { 0x357a },
                        "discovery did not use the appropriate fresh ID"
                    );
                    let reply = discovered(request, id, configuration);
                    ensure!(
                        peer.send_to(&reply, source)? == reply.len(),
                        "short fake discovery reply"
                    );
                    if mutations != 0 {
                        return Ok(mutations);
                    }
                }
                _ => anyhow::bail!("unexpected control or cleanup command at fake peer"),
            }
        }
        Ok(mutations)
    });
    let mut channel = PeerChannel {
        socket,
        peer: endpoint,
    };
    let mut progress = Progress::default();
    let result = assign(
        request,
        &mut channel,
        Budget::new(Instant::now() + request.timeout, &|| false),
        [0x3578, 0x3579, 0x357a],
        &mut progress,
        || Ok(()),
    );
    stop.store(true, Ordering::Release);
    let mutations = server
        .join()
        .map_err(|_| anyhow::anyhow!("fake UDP peer panicked"))??;
    result?;
    assert_eq!(mutations, 1);
    assert_eq!(
        progress
            .before
            .context("missing before observation")?
            .configuration,
        previous()
    );
    assert_eq!(
        progress
            .observed
            .context("missing final observation")?
            .configuration,
        request.configuration
    );
    assert_eq!(progress.ack, Some(endpoint));
    // The network transport is genuinely loopback; only the discovery's
    // reported incompatible/assigned subnets are synthetic. No host address,
    // physical camera or broadcast network was reconfigured by this fixture.
    assert!(
        progress
            .observed
            .context("missing final observation")?
            .reply_source
            .ip()
            .is_loopback()
    );
    Ok(())
}

#[test]
fn invalid_timeout_is_usage_before_deadline_arithmetic_or_socket_setup() {
    for timeout in [Duration::ZERO, Duration::from_secs(31), Duration::MAX] {
        let mut invalid = request();
        invalid.timeout = timeout;
        let error = run(invalid, || false).unwrap_err();
        assert_eq!(crate::types::classify(&error).kind, ErrorKind::Usage);
    }
}
