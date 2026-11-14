//! Cold, explicit temporary IPv4 assignment. Discovery is evidence; neither a
//! broadcast ACK nor the advisory runtime lock is device-side ownership fencing.
#![forbid(unsafe_code)]
use crate::{
    transport::gige::bootstrap::{self, Channel, Interface},
    types::{Categorized, ErrorKind, Usage},
};
use anyhow::{Context as _, Result, ensure};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    net::{Ipv4Addr, SocketAddrV4},
    time::{Duration, Instant},
};

const DISCOVERY: u16 = 2;
const FORCE_IP: u16 = 4;
const MAX_DATAGRAMS: usize = 1024;
const UDP_BUFFER: usize = 65_536;
const RESCAN: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct Configuration {
    pub address: Ipv4Addr,
    pub netmask: Ipv4Addr,
    pub gateway: Ipv4Addr,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Request {
    pub mac: [u8; 6],
    pub interface: Ipv4Addr,
    pub configuration: Configuration,
    pub timeout: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
struct Observation {
    configuration: Configuration,
    reply_source: SocketAddrV4,
}

#[derive(Clone, Copy)]
struct Budget<'a> {
    deadline: Instant,
    interrupted: &'a dyn Fn() -> bool,
}
impl<'a> Budget<'a> {
    const fn new(deadline: Instant, interrupted: &'a dyn Fn() -> bool) -> Self {
        Self {
            deadline,
            interrupted,
        }
    }
    fn check(self) -> Result<()> {
        if (self.interrupted)() {
            return Err(Categorized::error(
                ErrorKind::Interrupted,
                "ForceIP interrupted; no assignment retry or rollback was sent",
            ));
        }
        if Instant::now() >= self.deadline {
            return Err(Categorized::error(
                ErrorKind::Timeout,
                "ForceIP deadline expired; no assignment retry was sent",
            ));
        }
        Ok(())
    }
}

#[derive(Default)]
struct Progress {
    before: Option<Observation>,
    observed: Option<Observation>,
    ack: Option<SocketAddrV4>,
    send_attempted: bool,
    sent: bool,
    received: usize,
    ignored: usize,
}

pub(crate) fn parse_mac(text: &str) -> std::result::Result<[u8; 6], String> {
    if !(text.len() == 12 && text.bytes().all(|b| b.is_ascii_hexdigit())
        || text.len() == 17
            && text.bytes().enumerate().all(|(i, b)| {
                if i % 3 == 2 {
                    b == b':'
                } else {
                    b.is_ascii_hexdigit()
                }
            }))
    {
        return Err("MAC must be twelve hex digits or six colon-separated hex pairs".into());
    }
    let digits: Vec<_> = text.bytes().filter(|b| *b != b':').collect();
    let mut mac = [0; 6];
    for (byte, pair) in mac.iter_mut().zip(digits.as_chunks::<2>().0) {
        let text = std::str::from_utf8(pair).map_err(|e| e.to_string())?;
        *byte = u8::from_str_radix(text, 16).map_err(|e| e.to_string())?;
    }
    if mac == [0; 6] || mac[0] & 1 != 0 {
        return Err("MAC must be a nonzero unicast address".into());
    }
    Ok(mac)
}

const fn unicast(address: Ipv4Addr) -> bool {
    let first = address.octets()[0];
    first != 0 && first != 127 && first < 224
}

fn host(address: Ipv4Addr, mask: Ipv4Addr) -> bool {
    let inverse = !u32::from(mask);
    let host = u32::from(address) & inverse;
    host != 0 && host != inverse
}

fn subnet(a: Ipv4Addr, b: Ipv4Addr, mask: Ipv4Addr) -> bool {
    u32::from(a) & u32::from(mask) == u32::from(b) & u32::from(mask)
}

fn valid_mask(mask: Ipv4Addr) -> bool {
    let mask = u32::from(mask);
    let inverse = !mask;
    mask != 0 && inverse > 1 && inverse & inverse.wrapping_add(1) == 0
}

impl Request {
    pub(crate) fn validate(self) -> Result<()> {
        if self.mac == [0; 6] || self.mac[0] & 1 != 0 {
            return Err(Usage("ForceIP requires a nonzero unicast MAC".into()).into());
        }
        if self.timeout.is_zero() || self.timeout > Duration::from_secs(30) {
            return Err(
                Usage("ForceIP timeout must be greater than zero and at most 30s".into()).into(),
            );
        }
        let Configuration {
            address,
            netmask,
            gateway,
        } = self.configuration;
        if !unicast(self.interface)
            || !unicast(address)
            || self.interface == address
            || !valid_mask(netmask)
            || !host(address, netmask)
            || !host(self.interface, netmask)
            || !subnet(self.interface, address, netmask)
        {
            return Err(Usage("ForceIP requires distinct unicast host/interface addresses on the supplied contiguous IPv4 subnet (/1 through /30)".into()).into());
        }
        if !gateway.is_unspecified()
            && (!unicast(gateway)
                || !host(gateway, netmask)
                || !subnet(address, gateway, netmask)
                || gateway == address)
        {
            return Err(Usage(
                "ForceIP gateway must be 0.0.0.0 or another unicast host on the requested subnet"
                    .into(),
            )
            .into());
        }
        Ok(())
    }
}

fn current_interface(request: Request) -> Result<Interface> {
    let addresses =
        if_addrs::get_if_addrs().context("enumerating the requested ForceIP interface")?;
    if addresses
        .iter()
        .any(|i| i.ip() == request.configuration.address)
    {
        return Err(Usage("ForceIP camera address is already a local host address".into()).into());
    }
    let mut matches = Vec::new();
    for interface in addresses {
        let if_addrs::IfAddr::V4(address) = interface.addr else {
            continue;
        };
        if address.ip != request.interface {
            continue;
        }
        let index = interface.index.filter(|i| *i != 0).ok_or_else(|| {
            Categorized::error(
                ErrorKind::Unsupported,
                "the requested interface has no usable OS index",
            )
        })?;
        if !valid_mask(address.netmask)
            || !host(request.configuration.address, address.netmask)
            || !subnet(address.ip, request.configuration.address, address.netmask)
        {
            return Err(Usage("ForceIP camera address must be a usable host on the selected host interface's subnet too".into()).into());
        }
        let selected = Interface {
            name: interface.name,
            index,
            address: address.ip,
            netmask: address.netmask,
        };
        if !matches.contains(&selected) {
            matches.push(selected);
        }
    }
    if matches.len() != 1 {
        return Err(Usage(
            "ForceIP --interface must identify exactly one current local IPv4 interface".into(),
        )
        .into());
    }
    matches.pop().context("missing selected ForceIP interface")
}

fn known_owner(id: &str) -> Result<()> {
    for session in crate::ipc::sessions() {
        if session.cameras.iter().any(|c| c.named_by(id).is_some()) {
            return Err(Categorized::new(
                ErrorKind::CameraBusy,
                "ForceIP target is held by a known Capturefab session; disconnect it first",
            )
            .details(json!({"camera":id,"session":session.name,"pid":session.pid}))
            .into());
        }
    }
    Ok(())
}

pub(crate) fn run(request: Request, interrupted: fn() -> bool) -> Result<Value> {
    let started = Instant::now();
    request.validate()?;
    let deadline = started
        .checked_add(request.timeout)
        .ok_or_else(|| Usage("ForceIP deadline overflow".into()))?;
    let budget = Budget::new(deadline, &interrupted);
    let interface = current_interface(request)?;
    let id = format!("gige:{}", crate::types::hex(request.mac));
    known_owner(&id)?;
    let _reservation = crate::engine::reserve_network_camera(&id)?;
    budget.check()?;
    let mut socket = bootstrap::Socket::open(&interface)?;
    let mut random = [0; 2];
    getrandom::fill(&mut random)
        .map_err(|error| anyhow::anyhow!("generating ForceIP request IDs: {error}"))?;
    let first = u16::from_ne_bytes(random).max(1);
    let ids = [
        first,
        first.wrapping_add(1).max(1),
        first.wrapping_add(1).max(1).wrapping_add(1).max(1),
    ];
    let mut progress = Progress::default();
    let result = assign(request, &mut socket, budget, ids, &mut progress, || {
        known_owner(&id)?;
        if current_interface(request)? != interface {
            return Err(Categorized::error(
                ErrorKind::Unavailable,
                "ForceIP interface changed before assignment",
            ));
        }
        Ok(())
    });
    let summary = summary(request, &interface, &progress);
    match result {
        Ok(()) => Ok(summary),
        Err(error) => Err(crate::types::with_details(
            error,
            json!({"force_ip":summary}),
        )),
    }
}

fn summary(request: Request, interface: &Interface, progress: &Progress) -> Value {
    crate::types::without_nulls(json!({
        "camera":format!("gige:{}",crate::types::hex(request.mac)), "mac":crate::types::hex(request.mac),
        "interface":interface,"requested":request.configuration,"before":progress.before,"observed":progress.observed,
        "verification":if progress.observed.is_some() { "fresh_mac_and_configuration_discovery" } else { "unconfirmed" },
        "ack":if progress.ack.is_some() { "received" } else { "not_observed" },"ack_source":progress.ack,
        "mutation_send_attempted":progress.send_attempted,"mutation_datagrams":u8::from(progress.sent),
        "assignment_may_have_applied":progress.send_attempted,"received_datagrams":progress.received,"ignored_datagrams":progress.ignored,
        "reachability":"not_probed","temporary":true,
        "reservation":{"scope":"advisory_selected_runtime_native_mac","no_other_open_camera_required":true},
        "limitations":["does not exclude opaque GenTL aliases, external applications, other runtimes or native owners opened without a runtime lock", "discovery is unauthenticated; absence of a conflicting reply does not prove the address is unused"]
    }))
}

fn command(command: u16, id: u16) -> [u8; 8] {
    let mut packet = [0x42, 0x11, 0, 0, 0, 0, 0, 0];
    packet[2..4].copy_from_slice(&command.to_be_bytes());
    packet[6..8].copy_from_slice(&id.to_be_bytes());
    packet
}

fn encode(request: Request, id: u16) -> [u8; 64] {
    let mut packet = [0; 64];
    packet[..8].copy_from_slice(&command(FORCE_IP, id));
    packet[4..6].copy_from_slice(&56_u16.to_be_bytes());
    packet[10..16].copy_from_slice(&request.mac);
    packet[28..32].copy_from_slice(&request.configuration.address.octets());
    packet[44..48].copy_from_slice(&request.configuration.netmask.octets());
    packet[60..64].copy_from_slice(&request.configuration.gateway.octets());
    packet
}

fn send(channel: &mut impl Channel, packet: &[u8], budget: Budget<'_>) -> Result<()> {
    budget.check()?;
    let sent = channel.send(packet);
    budget.check()?;
    ensure!(sent? == packet.len(), "short GigE bootstrap datagram send");
    Ok(())
}

fn receive(
    channel: &mut impl Channel,
    buffer: &mut [u8],
    budget: Budget<'_>,
    progress: &mut Progress,
) -> Result<Option<(usize, SocketAddrV4)>> {
    budget.check()?;
    let received = channel.receive(buffer);
    budget.check()?;
    let Some(datagram) = received? else {
        std::thread::sleep(
            Duration::from_millis(2).min(budget.deadline.saturating_duration_since(Instant::now())),
        );
        return Ok(None);
    };
    progress.received += 1;
    if progress.received > MAX_DATAGRAMS {
        return Err(Categorized::error(
            ErrorKind::Incomplete,
            "ForceIP bootstrap traffic exceeded the 1024-datagram bound",
        ));
    }
    #[cfg(not(test))]
    let port = 3956;
    #[cfg(test)]
    let port = channel.test_source_port();
    if !datagram.selected_interface
        || datagram.source.port() != port
        || datagram.bytes > buffer.len()
    {
        progress.ignored += 1;
        return Ok(None);
    }
    Ok(Some((datagram.bytes, datagram.source)))
}

fn matched(packet: &[u8], command: u16, id: u16) -> bool {
    let Some(header) = packet.first_chunk::<8>() else {
        return false;
    };
    header[2..4] == command.to_be_bytes() && header[6..8] == id.to_be_bytes()
}

fn payload(packet: &[u8]) -> Result<&[u8]> {
    ensure!(packet.len() >= 8, "short GigE bootstrap acknowledgement");
    let length = usize::from(u16::from_be_bytes(packet[4..6].try_into()?));
    ensure!(
        packet.len() == 8 + length,
        "GigE bootstrap acknowledgement length mismatch"
    );
    let status = u16::from_be_bytes(packet[..2].try_into()?);
    if status != 0 {
        return Err(crate::transport::gige::bootstrap_status(status));
    }
    Ok(&packet[8..])
}

fn observation(packet: &[u8], source: SocketAddrV4) -> Result<([u8; 6], Observation)> {
    let payload = payload(packet)?
        .first_chunk::<0xf8>()
        .context("short GigE discovery configuration")?;
    let mac = payload[10..16].try_into()?;
    let address = Ipv4Addr::from(<[u8; 4]>::try_from(&payload[0x24..0x28])?);
    let netmask = Ipv4Addr::from(<[u8; 4]>::try_from(&payload[0x34..0x38])?);
    let gateway = Ipv4Addr::from(<[u8; 4]>::try_from(&payload[0x44..0x48])?);
    Ok((
        mac,
        Observation {
            configuration: Configuration {
                address,
                netmask,
                gateway,
            },
            reply_source: source,
        },
    ))
}

fn discovery(
    packet: &[u8],
    source: SocketAddrV4,
    request: Request,
    id: u16,
    progress: &mut Progress,
) -> Result<Option<Observation>> {
    if !matched(packet, DISCOVERY + 1, id) {
        progress.ignored += 1;
        return Ok(None);
    }
    let (mac, observed) = observation(packet, source)?;
    if mac != request.mac {
        if observed.configuration.address == request.configuration.address {
            return Err(Categorized::new(ErrorKind::CameraBusy, "another discovered MAC already reports the requested IPv4 address")
                .details(json!({"conflicting_mac":crate::types::hex(mac),"address":request.configuration.address})).into());
        }
        progress.ignored += 1;
        return Ok(None);
    }
    Ok(Some(observed))
}

fn assign(
    request: Request,
    channel: &mut impl Channel,
    budget: Budget<'_>,
    ids: [u16; 3],
    progress: &mut Progress,
    mut before_send: impl FnMut() -> Result<()>,
) -> Result<()> {
    budget.check()?;
    let preflight = Instant::now()
        .checked_add(request.timeout / 3)
        .context("ForceIP preflight deadline overflow")?
        .min(budget.deadline);
    send(channel, &command(DISCOVERY, ids[0]), budget)?;
    let mut buffer = vec![0; UDP_BUFFER];
    while Instant::now() < preflight {
        let Some((length, source)) = receive(channel, &mut buffer, budget, progress)? else {
            continue;
        };
        if let Some(observed) = discovery(&buffer[..length], source, request, ids[0], progress)? {
            if progress
                .before
                .is_some_and(|before| before.configuration != observed.configuration)
            {
                return Err(Categorized::error(
                    ErrorKind::CameraAmbiguous,
                    "target MAC reported changing or conflicting configurations before ForceIP",
                ));
            }
            progress.before = Some(observed);
        }
    }
    if progress.before.is_none() {
        return Err(Categorized::error(
            ErrorKind::CameraNotFound,
            "target MAC was not discovered on the selected interface; no ForceIP sent",
        ));
    }
    before_send()?;
    budget.check()?;
    progress.send_attempted = true;
    let sent = channel.send(&encode(request, ids[1]));
    if sent.as_ref().is_ok_and(|count| *count == 64) {
        progress.sent = true;
    }
    budget.check()?;
    ensure!(
        sent? == 64,
        "short ForceIP datagram send; assignment is unconfirmed"
    );
    let mut next_discovery = Instant::now();
    loop {
        budget.check()?;
        if Instant::now() >= next_discovery {
            send(channel, &command(DISCOVERY, ids[2]), budget)?;
            next_discovery = Instant::now()
                .checked_add(RESCAN)
                .context("ForceIP discovery deadline overflow")?;
        }
        let Some((length, source)) = receive(channel, &mut buffer, budget, progress)? else {
            continue;
        };
        let packet = &buffer[..length];
        if matched(packet, FORCE_IP + 1, ids[1]) {
            ensure!(
                payload(packet)?.is_empty(),
                "ForceIP acknowledgement has an unexpected payload"
            );
            progress.ack = Some(source);
        } else if let Some(observed) = discovery(packet, source, request, ids[2], progress)?
            && observed.configuration == request.configuration
        {
            budget.check()?;
            progress.observed = Some(observed);
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests;
