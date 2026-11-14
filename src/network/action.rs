//! Explicit GVCP actions. A successful ACK is not execution or a captured frame.
#![forbid(unsafe_code)]
use crate::{
    transport::gige::bootstrap::{self, Channel, Interface},
    types::{Categorized, ErrorKind, Usage},
};
use anyhow::{Context as _, Result};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    net::{Ipv4Addr, SocketAddrV4},
    time::{Duration, Instant},
};

const MAX_PEERS: usize = 32;
const MAX_DATAGRAMS: usize = 1024;
const UDP_BUFFER: usize = 65_536;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Mode {
    Acknowledged,
    SentOnly,
}

#[derive(Debug, Clone)]
pub(crate) struct Request {
    pub mode: Mode,
    pub interface: Ipv4Addr,
    pub device_key: u32,
    pub group_key: u32,
    pub group_mask: u32,
    /// Absolute device ticks supplied by the caller; `Some(0)` is still scheduled.
    pub action_time_ticks: Option<u64>,
    pub expected: Vec<Ipv4Addr>,
    pub timeout: Duration,
}

const fn unicast(address: Ipv4Addr) -> bool {
    let first = address.octets()[0];
    first != 0 && first < 224
}

fn valid_mask(mask: Ipv4Addr) -> bool {
    let inverse = !u32::from(mask);
    inverse > 1 && inverse < u32::MAX && inverse & inverse.wrapping_add(1) == 0
}

fn host_on_subnet(address: Ipv4Addr, interface: &Interface) -> bool {
    let mask = u32::from(interface.netmask);
    let host = u32::from(address) & !mask;
    host != 0 && host != !mask && u32::from(address) & mask == u32::from(interface.address) & mask
}

impl Request {
    const fn packet_len(&self) -> usize {
        if self.action_time_ticks.is_some() {
            28
        } else {
            20
        }
    }

    fn validate_bounds(&self) -> Result<()> {
        if self.timeout.is_zero() || self.timeout > Duration::from_secs(30) {
            return Err(
                Usage("action timeout must be greater than zero and at most 30s".into()).into(),
            );
        }
        if !unicast(self.interface) || self.group_mask == 0 {
            return Err(Usage(
                "action requires a unicast interface address and nonzero group mask".into(),
            )
            .into());
        }
        match self.mode {
            Mode::Acknowledged if self.expected.is_empty() || self.expected.len() > MAX_PEERS => {
                return Err(Usage(
                    "acknowledged action requires 1..32 explicit --expect addresses".into(),
                )
                .into());
            }
            Mode::SentOnly if !self.expected.is_empty() => {
                return Err(
                    Usage("sent-only action does not accept --expect addresses".into()).into(),
                );
            }
            _ => {}
        }
        Ok(())
    }

    pub(crate) fn validate(&self) -> Result<()> {
        self.validate_bounds()?;
        for (index, peer) in self.expected.iter().enumerate() {
            if !unicast(*peer) || *peer == self.interface || self.expected[..index].contains(peer) {
                return Err(Usage("action peers must be distinct unicast addresses other than the local interface".into()).into());
            }
        }
        Ok(())
    }

    fn validate_interface(&self, interface: &Interface) -> Result<()> {
        if interface.address != self.interface
            || interface.index == 0
            || interface.name.is_empty()
            || interface.name.len() > 1024
            || !valid_mask(interface.netmask)
            || !host_on_subnet(self.interface, interface)
            || self
                .expected
                .iter()
                .any(|peer| !host_on_subnet(*peer, interface))
        {
            return Err(Usage("action requires an indexed local interface with a contiguous /1..30 subnet and expected peers that are usable hosts on that subnet".into()).into());
        }
        Ok(())
    }
}

fn current_interface(request: &Request) -> Result<Interface> {
    let addresses = if_addrs::get_if_addrs().context("enumerating the action interface")?;
    if addresses.iter().any(|interface| {
        let std::net::IpAddr::V4(address) = interface.ip() else {
            return false;
        };
        request.expected.contains(&address)
    }) {
        return Err(Usage("an expected action peer is a local host address".into()).into());
    }
    let mut matches = Vec::new();
    for interface in addresses {
        let if_addrs::IfAddr::V4(address) = interface.addr else {
            continue;
        };
        if address.ip != request.interface {
            continue;
        }
        let selected = Interface {
            name: interface.name,
            index: interface.index.filter(|index| *index != 0).ok_or_else(|| {
                Categorized::error(
                    ErrorKind::Unsupported,
                    "the action interface has no usable OS index",
                )
            })?,
            address: address.ip,
            netmask: address.netmask,
        };
        request.validate_interface(&selected)?;
        if !matches.contains(&selected) {
            matches.push(selected);
        }
    }
    if matches.len() != 1 {
        return Err(Usage(
            "action --interface must identify exactly one current local IPv4 interface".into(),
        )
        .into());
    }
    matches.pop().context("missing action interface")
}

#[derive(Clone, Copy)]
struct Budget<'a> {
    deadline: Instant,
    interrupted: &'a dyn Fn() -> bool,
    now: &'a dyn Fn() -> Instant,
}

impl Budget<'_> {
    fn check(self) -> Result<()> {
        if (self.interrupted)() {
            return Err(Categorized::error(
                ErrorKind::Interrupted,
                "action interrupted; no retry was sent",
            ));
        }
        if (self.now)() >= self.deadline {
            return Err(Categorized::error(
                ErrorKind::Timeout,
                "action deadline expired; no retry was sent",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
struct Reply {
    source: SocketAddrV4,
    status_raw: u16,
    status: &'static str,
}

#[derive(Default)]
struct Progress {
    attempted: bool,
    sent: bool,
    completed: bool,
    received: usize,
    ignored: usize,
    duplicates: usize,
    replies: Vec<Reply>,
}

const fn status_name(status: u16) -> &'static str {
    match status {
        0 => "success",
        0x8001 => "not_implemented",
        0x8002 => "invalid_parameter",
        0x8006 => "access_denied",
        0x8007 => "busy",
        0x8013 => "no_reference_time",
        0x8015 => "overflow",
        0x8016 => "action_late",
        _ => "unknown",
    }
}

fn encode(request: &Request, id: u16) -> [u8; 28] {
    let mut packet = [0; 28];
    let scheduled = request.action_time_ticks.is_some();
    packet[..6].copy_from_slice(&[
        0x42,
        u8::from(request.mode == Mode::Acknowledged) | if scheduled { 0x80 } else { 0 },
        1,
        0,
        0,
        if scheduled { 20 } else { 12 },
    ]);
    packet[6..8].copy_from_slice(&id.to_be_bytes());
    packet[8..12].copy_from_slice(&request.device_key.to_be_bytes());
    packet[12..16].copy_from_slice(&request.group_key.to_be_bytes());
    packet[16..20].copy_from_slice(&request.group_mask.to_be_bytes());
    if let Some(ticks) = request.action_time_ticks {
        packet[20..28].copy_from_slice(&ticks.to_be_bytes());
    }
    packet
}

fn rejected(kind: ErrorKind, reason: &'static str, message: &str) -> anyhow::Error {
    Categorized::new(kind, message)
        .details(json!({"action_failure":reason}))
        .into()
}

fn exchange(
    request: &Request,
    channel: &mut impl Channel,
    budget: Budget<'_>,
    id: u16,
    progress: &mut Progress,
    mut before_send: impl FnMut() -> Result<()>,
) -> Result<()> {
    budget.check()?;
    before_send()?;
    budget.check()?;
    progress.attempted = true;
    let packet = encode(request, id);
    let packet_len = request.packet_len();
    let sent = channel.send(&packet[..packet_len]);
    progress.sent = sent.as_ref().is_ok_and(|bytes| *bytes == packet_len);
    budget.check()?;
    if sent.context("sending the sole action datagram; it may have been issued")? != packet_len {
        return Err(rejected(
            ErrorKind::Incomplete,
            "short_send",
            "short action datagram send; execution is unconfirmed",
        ));
    }
    if request.mode == Mode::SentOnly {
        budget.check()?;
        progress.completed = true;
        return Ok(());
    }
    let mut buffer = vec![0; UDP_BUFFER];
    loop {
        budget.check()?;
        if progress.received == MAX_DATAGRAMS {
            return Err(rejected(
                ErrorKind::Incomplete,
                "traffic_limit",
                "action response traffic reached the 1024-datagram bound",
            ));
        }
        let received = channel.receive(&mut buffer);
        if matches!(&received, Ok(Some(_))) {
            progress.received += 1;
        }
        budget.check()?;
        let Some(datagram) = received.context("receiving action acknowledgements")? else {
            std::thread::sleep(
                Duration::from_millis(2)
                    .min(budget.deadline.saturating_duration_since((budget.now)())),
            );
            continue;
        };
        #[cfg(not(test))]
        let port = 3956;
        #[cfg(test)]
        let port = channel.test_source_port();
        if !datagram.selected_interface
            || datagram.source.port() != port
            || !request.expected.contains(datagram.source.ip())
        {
            progress.ignored += 1;
            continue;
        }
        let packet = &buffer[..datagram.bytes.min(buffer.len())];
        let Some(header) = packet.first_chunk::<8>() else {
            progress.ignored += 1;
            continue;
        };
        if header[2..4] != [1, 1] || header[6..8] != id.to_be_bytes() {
            progress.ignored += 1;
            continue;
        }
        if datagram.bytes != 8 || header[4..6] != [0, 0] {
            return Err(rejected(
                ErrorKind::Incomplete,
                "malformed_reply",
                "correlated action acknowledgement must be exactly eight bytes with no body",
            ));
        }
        let status = u16::from_be_bytes([header[0], header[1]]);
        if let Some(previous) = progress
            .replies
            .iter()
            .find(|reply| reply.source == datagram.source)
        {
            if previous.status_raw != status {
                return Err(crate::types::with_details(
                    rejected(
                        ErrorKind::CameraAmbiguous,
                        "response_conflict",
                        "one action peer returned conflicting acknowledgement statuses",
                    ),
                    json!({"peer":datagram.source,"previous_status_raw":previous.status_raw,"conflicting_status_raw":status}),
                ));
            }
            progress.duplicates += 1;
        } else {
            progress.replies.push(Reply {
                source: datagram.source,
                status_raw: status,
                status: status_name(status),
            });
        }
        budget.check()?;
        if status != 0 {
            return Err(rejected(
                ErrorKind::OperationFailed,
                "remote_status",
                "an action peer returned a non-success GVCP status",
            ));
        }
        if progress.replies.len() == request.expected.len() {
            budget.check()?;
            progress.completed = true;
            return Ok(());
        }
    }
}

fn summary(request: &Request, interface: &Interface, id: u16, progress: &Progress) -> Value {
    let missing: Vec<_> = request
        .expected
        .iter()
        .filter(|peer| {
            !progress
                .replies
                .iter()
                .any(|reply| reply.source.ip() == *peer)
        })
        .collect();
    let outcome = if progress.completed {
        if request.mode == Mode::SentOnly {
            "sent_without_ack_confirmation"
        } else {
            "acknowledged"
        }
    } else {
        "unconfirmed"
    };
    let mut value = json!({
        "mode":request.mode,"action":if request.action_time_ticks.is_some() { "scheduled" } else { "immediate" },"outcome":outcome,
        "interface":interface,"request_id":id,"expected":request.expected,
        "responses":progress.replies,"missing":missing,
        "verification":if request.mode == Mode::SentOnly { "not_requested" } else if progress.completed { "strict_action_acknowledgements" } else { "unconfirmed" },
        "mutation_send_attempted":progress.attempted,"mutation_datagrams":u8::from(progress.sent),
        "bytes":if progress.sent {request.packet_len()} else {0},"action_may_have_been_issued":progress.attempted,
        "received_datagrams":progress.received,"ignored_datagrams":progress.ignored,"duplicate_datagrams":progress.duplicates,
        "execution":"not_verified","frame_arrival":"not_verified","synchronization":"not_verified"
    });
    if let Some(ticks) = request.action_time_ticks {
        value["action_time_ticks"] = json!(ticks);
    }
    value
}

fn result(
    request: &Request,
    interface: &Interface,
    id: u16,
    progress: &Progress,
    result: Result<()>,
    budget: Budget<'_>,
) -> Result<Value> {
    let mut value = summary(request, interface, id, progress);
    match result.and_then(|()| budget.check()) {
        Ok(()) => Ok(value),
        Err(error) => {
            value["outcome"] = json!("unconfirmed");
            if request.mode == Mode::Acknowledged {
                value["verification"] = json!("unconfirmed");
            }
            Err(crate::types::with_details(error, json!({"action":value})))
        }
    }
}

pub(crate) fn run(request: &Request, interrupted: fn() -> bool) -> Result<Value> {
    let started = Instant::now();
    request.validate()?;
    let deadline = started
        .checked_add(request.timeout)
        .ok_or_else(|| Usage("action deadline overflow".into()))?;
    let budget = Budget {
        deadline,
        interrupted: &interrupted,
        now: &Instant::now,
    };
    budget.check()?;
    let interface = current_interface(request)?;
    budget.check()?;
    let mut bytes = [0; 2];
    getrandom::fill(&mut bytes)
        .map_err(|error| anyhow::anyhow!("generating an action request ID: {error}"))?;
    let id = u16::from_ne_bytes(bytes).max(1);
    let mut progress = Progress::default();
    let attempt = (|| {
        budget.check()?;
        let mut socket = bootstrap::Socket::open(&interface)?;
        exchange(request, &mut socket, budget, id, &mut progress, || {
            if current_interface(request)? != interface {
                return Err(Categorized::error(
                    ErrorKind::Unavailable,
                    "action interface changed before sending",
                ));
            }
            Ok(())
        })
    })();
    result(request, &interface, id, &progress, attempt, budget)
}

/// Supplies only a test channel; release code has no destination/port override.
#[cfg(test)]
pub(crate) fn test_exchange(
    request: &Request,
    interface: &Interface,
    channel: &mut impl Channel,
    request_id: u16,
    interrupted: &dyn Fn() -> bool,
) -> Result<Value> {
    request.validate()?;
    test_core(request, interface, channel, request_id, interrupted)
}

/// The synthetic camera shares loopback with the host. Only this test seam
/// allows that self-peer; packet correlation and the exchange are unchanged.
#[cfg(test)]
pub(crate) fn test_fixture_exchange(
    request: &Request,
    interface: &Interface,
    channel: &mut impl Channel,
    request_id: u16,
    interrupted: &dyn Fn() -> bool,
) -> Result<Value> {
    request.validate_bounds()?;
    anyhow::ensure!(
        request.mode == Mode::Acknowledged
            && request.interface.is_loopback()
            && request.expected == [request.interface],
        "acknowledged fixture action requires exactly the local loopback peer"
    );
    test_core(request, interface, channel, request_id, interrupted)
}

#[cfg(test)]
fn test_core(
    request: &Request,
    interface: &Interface,
    channel: &mut impl Channel,
    request_id: u16,
    interrupted: &dyn Fn() -> bool,
) -> Result<Value> {
    let started = Instant::now();
    request.validate_interface(interface)?;
    anyhow::ensure!(request_id != 0, "action request ID must be nonzero");
    let deadline = started
        .checked_add(request.timeout)
        .ok_or_else(|| Usage("action deadline overflow".into()))?;
    let budget = Budget {
        deadline,
        interrupted,
        now: &Instant::now,
    };
    let mut progress = Progress::default();
    let attempt = exchange(request, channel, budget, request_id, &mut progress, || {
        Ok(())
    });
    result(request, interface, request_id, &progress, attempt, budget)
}

#[cfg(test)]
mod tests;
