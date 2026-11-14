//! A native Rust `GigE` Vision control and image transport.
//!
//! Wire constants and layouts follow the `GigE` Vision bootstrap/GVCP/GVSP
//! protocols (also documented in Aravis' protocol headers). This implementation
//! does not link to, embed, or translate Aravis code.
#![deny(clippy::missing_const_for_fn)]
use super::chunks::{ByteOrder, ChunkData, ImageLayout};
use crate::copies::Site;
use crate::framebuf::{PoolVec, pool};
use crate::types::{
    Backend, CameraInfo, Categorized, Frame, RegisterIo, Transport, TransportStats,
    control_remaining,
};
use crate::xml_cache;
use anyhow::{Context as _, Result, anyhow, bail, ensure};
use std::cell::OnceCell;
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::io::{Cursor, ErrorKind, Read};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[cfg(test)]
pub(crate) mod action_workflow_tests;
#[cfg(any(test, target_os = "linux"))]
mod batch;
pub(crate) mod bootstrap;
pub(crate) mod monitor;
mod multipart;
mod ownership;
use ownership::Lease;

/// Whether the native controller permanently retired this ownership epoch.
pub(crate) fn ownership_retired(error: &anyhow::Error) -> bool {
    ownership::retired(error)
}
mod xml_http;

/// Shared bounded retrieval for vendor-advertised plain HTTP XML locations.
pub(super) fn fetch_xml_http(url: &str, timeout: Duration) -> Result<(String, Vec<u8>)> {
    xml_http::get(url, timeout)
}

const GVCP_PORT: u16 = 3956;
const DISCOVERY: u16 = 0x0002;
const PACKET_RESEND: u16 = 0x0040;
const READ_REGISTER: u16 = 0x0080;
const WRITE_REGISTER: u16 = 0x0082;
const READ_MEMORY: u16 = 0x0084;
const WRITE_MEMORY: u16 = 0x0086;
const PENDING_ACK: u16 = 0x0089;
const DEVICE_MODE: u32 = 0x0004;
const CONTROL_PRIVILEGE: u32 = 0x0a00;
const GVCP_CAPABILITY: u32 = 0x0934;
const GVCP_CONFIGURATION: u32 = 0x0954;
const PENDING_TIMEOUT: u32 = 0x0958;
const HEARTBEAT_TIMEOUT: u32 = 0x0938;
const PRIMARY_PORT: u32 = 0x0a04;
const PRIMARY_IP: u32 = 0x0a14;
const SWITCHOVER_KEY: u32 = 0x095c;
const OWNER_ENDPOINT: u32 = 0x0800_0000;
const DISABLE_CAPABILITY: u32 = 0x2000_0000;
const HEARTBEAT_DISABLE: u32 = 0x1;
const TIMESTAMP_HIGH: u32 = 0x093c;
const TIMESTAMP_LOW: u32 = 0x0940;
/// Timestamp control (bit 30 latches the clock) and the latched value.
const TIMESTAMP_CONTROL: u32 = 0x0944;
const TIMESTAMP_LATCH: u32 = 0x0000_0002;
const TIMESTAMP_VALUE_HIGH: u32 = 0x0948;
const TIMESTAMP_VALUE_LOW: u32 = 0x094c;
const STREAM_CAPABILITY: u32 = 0x092c;
const STREAM_PORT: u32 = 0x0d00;
const STREAM_PACKET_SIZE: u32 = 0x0d04;
const STREAM_ADDRESS: u32 = 0x0d18;
const STREAM_SOURCE_PORT: u32 = 0x0d1c;
const SCPS_FIRE: u32 = 0x8000_0000;
const SCPS_NO_FRAGMENT: u32 = 0x4000_0000;
const SCPS_BIG_ENDIAN: u32 = 0x2000_0000;
const PACKET_SIZE_MAX: u32 = 9000;
const MONO16: u32 = 0x0110_0007;
const MEMORY_CHUNK: usize = 512;
const MAX_XML: usize = 16 * 1024 * 1024;
const MAX_PAYLOAD: usize = 128 * 1024 * 1024;
const MAX_FRAME_PACKETS: usize = 262_144;
const MAX_INCOMPLETE_FRAMES: usize = 4;
const CLOSED_BLOCKS: usize = 64;
const POLL: Duration = Duration::from_millis(10);
const FRAME_IDLE: Duration = Duration::from_millis(250);
const RESEND_IDLE: Duration = Duration::from_millis(10);
const RESEND_RETRY: Duration = Duration::from_millis(20);
const RESEND_ROUNDS: u8 = 3;
const RESEND_RUNS: usize = 32;
const RESEND_BUDGET: f64 = 4096.0;
const TRAVERSAL: Duration = Duration::from_secs(10);
/// The most packets taken in at once when settings change: a 64 MiB
/// receive buffer of 1500-byte packets.
const MAX_TAKEN_IN: usize = 50_000;
/// `CAPTUREFAB_GVSP_SCATTER=0` receives every GVSP packet whole and copies
/// its payload into its frame, as before payloads were received in place.
pub(crate) const SCATTER_ENV: &str = "CAPTUREFAB_GVSP_SCATTER";

/// Whether payload packets are received in place, as the setting says.
fn scatter(setting: Option<&std::ffi::OsStr>) -> bool {
    setting.is_none_or(|value| value != "0")
}

const fn be16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_be_bytes([bytes[offset], bytes[offset + 1]])
}
fn be32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes(
        bytes[offset..offset + 4]
            .try_into()
            .expect("checked packet"),
    )
}
fn be64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_be_bytes(
        bytes[offset..offset + 8]
            .try_into()
            .expect("checked packet"),
    )
}
/// A tick count of the camera's clock in nanoseconds.
fn ticks_ns(ticks: u64, frequency: u64) -> u64 {
    u64::try_from((u128::from(ticks) * 1_000_000_000) / u128::from(frequency.max(1)))
        .unwrap_or(u64::MAX)
}
fn bounded_timeout(timeout: Duration) -> Duration {
    timeout.clamp(Duration::from_millis(1), Duration::from_secs(30))
}
fn is_receive_timeout(error: &std::io::Error) -> bool {
    matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut)
}
fn command_packet(command: u16, id: u16, flags: u8, payload: &[u8]) -> Result<Vec<u8>> {
    let length = u16::try_from(payload.len()).context("GVCP command payload exceeds u16 length")?;
    let mut packet = Vec::with_capacity(8 + payload.len());
    packet.extend([0x42, flags]);
    packet.extend(command.to_be_bytes());
    packet.extend(length.to_be_bytes());
    packet.extend(id.to_be_bytes());
    packet.extend(payload);
    Ok(packet)
}
fn resend_packet(id: u16, block: u64, extended: bool, first: u32, last: u32) -> Vec<u8> {
    let range = [first.to_be_bytes(), last.to_be_bytes()].concat();
    let payload = if extended {
        [&[0; 4][..], &range, &block.to_be_bytes()].concat()
    } else {
        [&[0, 0][..], &block.to_be_bytes()[6..], &range].concat()
    };
    command_packet(PACKET_RESEND, id, if extended { 0x10 } else { 0 }, &payload)
        .expect("resend commands have a fixed payload of at most 20 bytes")
}

struct Ack<'a> {
    status: u16,
    command: u16,
    id: u16,
    payload: &'a [u8],
}
fn parse_ack(packet: &[u8]) -> Result<Ack<'_>> {
    ensure!(packet.len() >= 8, "truncated GVCP acknowledgement header");
    let length = be16(packet, 4) as usize;
    ensure!(
        packet.len() == 8 + length,
        "GVCP acknowledgement length mismatch"
    );
    Ok(Ack {
        status: be16(packet, 0),
        command: be16(packet, 2),
        id: be16(packet, 6),
        payload: &packet[8..],
    })
}

/// A GVCP command that went unanswered through all its attempts.
#[derive(Debug)]
pub(crate) struct GvcpTimeout {
    pub(crate) command: u16,
    pub(crate) waited: Duration,
}
impl std::fmt::Display for GvcpTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "GVCP command 0x{:04x} timed out after {:?} (three attempts); check camera address, network and firewall",
            self.command, self.waited
        )
    }
}
impl std::error::Error for GvcpTimeout {}

#[derive(Debug)]
pub(crate) struct GvcpStatus(pub(crate) u16);
impl std::fmt::Display for GvcpStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let description = match self.0 {
            0x8001 => "command not implemented",
            0x8002 => "invalid parameter",
            0x8003 => "invalid address/access",
            0x8004 => "write protected",
            0x8005 => "unaligned address",
            0x8006 => "access denied (another controller may own the camera)",
            0x8007 => "camera busy",
            0x800c => "packet unavailable",
            0x800d => "data overrun",
            _ => "camera rejected command",
        };
        write!(f, "GVCP status 0x{:04x}: {description}", self.0)
    }
}
impl std::error::Error for GvcpStatus {}

pub(crate) fn bootstrap_status(status: u16) -> anyhow::Error {
    GvcpStatus(status).into()
}

/// A rejected GVCP command still establishes that the camera replied. Preserve
/// that distinction through contextual errors from repeated network probes.
pub(crate) fn is_control_rejection(error: &anyhow::Error) -> bool {
    error.downcast_ref::<GvcpStatus>().is_some()
}

/// A NUL-padded string field a device sent, up to its first NUL. Any host
/// on the network can answer discovery, so control characters, which could
/// steer the terminal that shows the name, do not survive.
fn fixed_text(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes.split(|b| *b == 0).next().unwrap_or_default());
    crate::types::printable_line(&text).trim().to_owned()
}
fn discovery_info(payload: &[u8], source: Ipv4Addr) -> Result<CameraInfo> {
    ensure!(payload.len() >= 0xf8, "truncated GigE discovery data");
    let reported = Ipv4Addr::from(be32(payload, 0x24));
    // A valid unicast source is the address that can actually be reached; some
    // devices report zero while changing IP configuration.
    let address = if source.is_unspecified() {
        reported
    } else {
        source
    };
    let serial = fixed_text(&payload[0xd8..0xe8]);
    let mac = ((u64::from(be32(payload, 8)) & 0xffff) << 32) | u64::from(be32(payload, 12));
    Ok(CameraInfo {
        id: if mac == 0 {
            format!("gige:{address}")
        } else {
            format!("gige:{mac:012x}")
        },
        transport: Transport::GigE,
        vendor: fixed_text(&payload[0x48..0x68]),
        model: fixed_text(&payload[0x68..0x88]),
        serial,
        address: Some(address.to_string()),
        profile: crate::camera_profile::CameraProfile::default(),
    })
}

/// A discovery socket bound to 0.0.0.0 but pinned to one interface, so that
/// its limited broadcast leaves on that interface and broadcast
/// acknowledgements from cameras on foreign subnets still reach it.
#[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
fn interface_pinned_socket(
    index: u32,
    #[cfg(any(target_os = "linux", target_os = "android"))] name: &str,
    #[cfg(target_os = "macos")] _name: &str,
) -> Option<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).ok()?;
    let pinned = socket.bind_device_by_index_v4(Some(std::num::NonZeroU32::new(index)?));
    // Kernels before 5.0 lack SO_BINDTOIFINDEX; SO_BINDTODEVICE needs no
    // privilege from 5.7.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let pinned = pinned.or_else(|_| socket.bind_device(Some(name.as_bytes())));
    pinned.ok()?;
    socket
        .bind(&SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0).into())
        .ok()?;
    Some(socket.into())
}
#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
const fn interface_pinned_socket(_index: u32, _name: &str) -> Option<UdpSocket> {
    None
}

/// Discover cameras on every usable IPv4 interface, within one shared deadline.
/// Directed and limited broadcasts are sent from sockets tied to each
/// interface so that multi-NIC hosts do not silently discover only the
/// default-route network, and cameras whose address does not match the
/// interface's subnet (which ignore directed broadcasts) are still listed.
pub(crate) fn discover(timeout: Duration) -> Result<Vec<CameraInfo>> {
    discover_until(timeout, &mut |_| false)
}
/// `discover`, telling `found` of each camera as it answers; it returns at
/// once, with what answered so far, when `found` says that was the one.
pub(crate) fn discover_until(
    timeout: Duration,
    found: &mut dyn FnMut(&CameraInfo) -> bool,
) -> Result<Vec<CameraInfo>> {
    let deadline = Instant::now() + bounded_timeout(timeout);
    let request = command_packet(DISCOVERY, 0xffff, 0x11, &[])?;
    let mut sockets = Vec::new();
    let mut local_addresses = HashSet::new();
    let mut pinned_interfaces = HashSet::new();
    // Set when a send failed in a way macOS Local Network privacy explains.
    let mut hint = None;
    for interface in if_addrs::get_if_addrs().context("enumerating IPv4 interfaces")? {
        let if_addrs::IfAddr::V4(v4) = &interface.addr else {
            continue;
        };
        if v4.ip.is_unspecified() || !local_addresses.insert(v4.ip) {
            continue;
        }
        let mut send = |socket: &UdpSocket, to: Ipv4Addr| match socket
            .send_to(&request, SocketAddrV4::new(to, GVCP_PORT))
        {
            Ok(_) => true,
            Err(error) => {
                hint = hint.or_else(|| crate::network::local_network_hint(&error, Some(to.into())));
                false
            }
        };
        let pinned = (!v4.ip.is_loopback() && v4.broadcast.is_some())
            .then_some(interface.index)
            .flatten()
            .filter(|index| pinned_interfaces.insert(*index))
            .and_then(|index| interface_pinned_socket(index, &interface.name))
            .filter(|socket| socket.set_broadcast(true).is_ok())
            .filter(|socket| socket.set_nonblocking(true).is_ok());
        let pinned_sent = pinned
            .as_ref()
            .is_some_and(|socket| send(socket, Ipv4Addr::BROADCAST));
        if pinned_sent {
            sockets.extend(pinned);
        }
        let Ok(socket) = UdpSocket::bind(SocketAddrV4::new(v4.ip, 0)) else {
            continue;
        };
        if socket.set_broadcast(true).is_err() || socket.set_nonblocking(true).is_err() {
            continue;
        }
        // Probe the local interface too: camera simulators and software GigE
        // producers often bind to a unicast or loopback interface.
        let mut sent = send(&socket, v4.ip);
        if let Some(broadcast) = v4.broadcast {
            sent |= send(&socket, broadcast);
            // A source-bound limited broadcast leaves on this interface too,
            // where no pinned socket is available.
            if !pinned_sent {
                sent |= send(&socket, Ipv4Addr::BROADCAST);
            }
        }
        if sent {
            sockets.push(socket);
        }
    }
    // This also covers interface APIs that omit directed broadcast addresses.
    if let Ok(socket) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
        && socket.set_broadcast(true).is_ok()
        && socket.set_nonblocking(true).is_ok()
    {
        match socket.send_to(&request, (Ipv4Addr::BROADCAST, GVCP_PORT)) {
            Ok(_) => sockets.push(socket),
            Err(error) => {
                hint = hint.or_else(|| {
                    crate::network::local_network_hint(&error, Some(Ipv4Addr::BROADCAST.into()))
                });
            }
        }
    }
    if sockets.is_empty() {
        bail!(
            "no usable IPv4 broadcast socket; {}",
            hint.unwrap_or("check network interfaces")
        );
    }
    let mut cameras = BTreeMap::new();
    let mut packet = [0u8; 2048];
    while Instant::now() < deadline {
        let mut received = false;
        for socket in &sockets {
            // Bound per-socket work as well as elapsed time, even on a noisy LAN.
            for _ in 0..64 {
                match socket.recv_from(&mut packet) {
                    Ok((length, SocketAddr::V4(source))) => {
                        received = true;
                        if source.port() != GVCP_PORT {
                            continue;
                        }
                        let Ok(ack) = parse_ack(&packet[..length]) else {
                            continue;
                        };
                        if ack.status != 0 || ack.command != DISCOVERY + 1 || ack.id != 0xffff {
                            continue;
                        }
                        if let Ok(info) = discovery_info(ack.payload, *source.ip()) {
                            let wanted = !cameras.contains_key(&info.id) && found(&info);
                            cameras.insert(info.id.clone(), info);
                            if wanted {
                                return Ok(cameras.into_values().collect());
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(error) if is_receive_timeout(&error) => break,
                    Err(_) => break,
                }
                if Instant::now() >= deadline {
                    break;
                }
            }
        }
        if !received {
            thread::sleep(
                Duration::from_millis(2).min(deadline.saturating_duration_since(Instant::now())),
            );
        }
    }
    // Nothing found after macOS refused some of the broadcasts: say why
    // instead of reporting an empty network.
    if let Some(hint) = hint
        && cameras.is_empty()
    {
        bail!("some GigE Vision discovery broadcasts could not be sent; {hint}");
    }
    Ok(cameras.into_values().collect())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Permit {
    Normal,
    Probe,
    Managed,
    Claim,
    Release,
}
struct ControlReply {
    payload: Vec<u8>,
    first_send: u64,
}
pub(crate) fn managed_range(address: u32, length: usize) -> bool {
    let end = u64::from(address) + length as u64;
    [
        HEARTBEAT_TIMEOUT,
        GVCP_CONFIGURATION,
        CONTROL_PRIVILEGE,
        SWITCHOVER_KEY,
    ]
    .into_iter()
    .any(|register| u64::from(register) < end && u64::from(register) + 4 > u64::from(address))
}
struct Control {
    lease: Option<Arc<Lease>>,
    owner_endpoint: Option<SocketAddrV4>,
    // Scoped only while the caller owns the Control mutex; heartbeat never inherits it.
    operation_deadline: Option<Instant>,
    disable_supported: bool,
    register_order_known: bool,
    #[cfg(test)]
    lease_clock: Option<Arc<std::sync::atomic::AtomicU64>>,
    socket: UdpSocket,
    timeout: Duration,
    next_id: u16,
    write_memory: bool,
    /// The device's maximum command response time (Pending Timeout register).
    response: Duration,
    /// How long `PENDING_ACKs` may extend a command past its deadline; `None`
    /// keeps every command within its deadline.
    pending_limit: Option<Duration>,
    /// The GVCP configuration to restore after enabling `PENDING_ACK`.
    restore_configuration: Option<u32>,
    /// Four-byte accesses use READREG/WRITEREG with network-order data, as
    /// `GenICam` schema 1.0 devices expect; otherwise memory accesses pass
    /// camera-order data as-is.
    legacy_registers: bool,
    little_endian: bool,
}
impl Control {
    fn connect(address: SocketAddrV4, timeout: Duration) -> Result<Self> {
        let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).context("opening GVCP socket")?;
        socket
            .connect(address)
            .context("routing GVCP socket to camera")?;
        Ok(Self {
            lease: None,
            owner_endpoint: None,
            operation_deadline: None,
            disable_supported: false,
            register_order_known: false,
            #[cfg(test)]
            lease_clock: None,
            socket,
            timeout: bounded_timeout(timeout),
            next_id: 1,
            write_memory: true,
            response: Duration::ZERO,
            pending_limit: None,
            restore_configuration: None,
            legacy_registers: true,
            little_endian: false,
        })
    }
    /// Wait for the device's advertised response time before retrying, so
    /// slow commands are not retransmitted while they execute.
    fn retry_wait(&self) -> Duration {
        (self.timeout / 3)
            .max(self.response + self.response.min(Duration::from_millis(100)))
            .max(Duration::from_millis(1))
    }
    /// The time for three attempts, before any `PENDING_ACK` extension.
    fn command_time(&self) -> Duration {
        self.timeout.max(self.retry_wait() * 3)
    }
    fn transact(&mut self, command: u16, payload: &[u8]) -> Result<Vec<u8>> {
        self.transact_owned(command, payload, Permit::Normal)
            .map(|reply| reply.payload)
    }
    fn check_operation(&self) -> Result<()> {
        self.operation_deadline
            .map_or(Ok(()), |stop| control_remaining(stop).map(|_| ()))
    }
    // Apply the caller's cap to kernel send-buffer waits too. Socket options
    // are local state; restore them before releasing the exclusive Control lock.
    fn with_send_deadline<T>(&self, action: impl FnOnce() -> Result<T>) -> Result<T> {
        let Some(deadline) = self.operation_deadline else {
            return action();
        };
        let mut left = control_remaining(deadline)?;
        if let Some(lease) = &self.lease
            && let Some(remaining) = lease.remaining()?
        {
            left = left.min(remaining);
        }
        let previous = self.socket.write_timeout()?;
        self.socket.set_write_timeout(Some(left))?;
        let result = action();
        let restored = self
            .socket
            .set_write_timeout(previous)
            .context("restoring GVCP send timeout");
        match (result, restored) {
            (Err(error), Err(restoration)) => Err(error.context(format!(
                "GVCP local timeout restoration also failed: {restoration:#}"
            ))),
            (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
            (Ok(value), Ok(())) => {
                self.check_operation()?;
                Ok(value)
            }
        }
    }
    fn transact_owned(
        &mut self,
        command: u16,
        payload: &[u8],
        permit: Permit,
    ) -> Result<ControlReply> {
        let lease = self.lease.clone();
        if lease.is_some() {
            if command == WRITE_MEMORY {
                ensure!(payload.len() >= 4, "invalid memory write request");
                if managed_range(be32(payload, 0), payload.len() - 4) {
                    return Err(Categorized::error(
                        crate::types::ErrorKind::Unsupported,
                        "mixed or memory-level controller-register writes are unsupported; write one complete managed register",
                    ));
                }
            }
            if command == WRITE_REGISTER {
                ensure!(
                    payload.len().is_multiple_of(8),
                    "invalid register write request"
                );
                for pair in payload.as_chunks::<8>().0 {
                    if managed_range(be32(pair, 0), 4) && permit == Permit::Normal {
                        return Err(Categorized::error(
                            crate::types::ErrorKind::Unsupported,
                            "controller-register writes require a verified ownership transition",
                        ));
                    }
                }
            }
        }
        if let Some(lease) = &lease {
            lease.check()?;
            if permit == Permit::Normal && lease.needs_probe(self.command_time())? {
                self.probe_owner()?;
            }
        }
        self.check_operation()?;
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let packet = command_packet(command, id, 1, payload)?;
        let retry_wait = self.retry_wait();
        let start = Instant::now();
        let command_deadline = start + self.command_time();
        let mut deadline = self
            .operation_deadline
            .map_or(command_deadline, |stop| stop.min(command_deadline));
        let pending_deadline = self.pending_limit.map_or(command_deadline, |limit| {
            command_deadline.max(start + limit)
        });
        let limit = self
            .operation_deadline
            .map_or(pending_deadline, |stop| stop.min(pending_deadline));
        let mut buffer = [0u8; 2048];
        let mut first_send = None;
        for attempt in 0..if matches!(permit, Permit::Release | Permit::Claim) {
            1
        } else {
            3
        } {
            if Instant::now() >= deadline {
                break;
            }
            let sent = self.with_send_deadline(|| {
                self.check_operation()?;
                let sent = lease.as_ref().map_or(Ok(0), |lease| lease.send_clock())?;
                // Release is final even if this send or its ACK fails. A second
                // datagram could reach a camera already claimed by another app.
                if permit == Permit::Release
                    && let Some(lease) = &lease
                {
                    lease.release()?;
                }
                self.socket.send(&packet).map_err(|error| {
                    // SO_SNDTIMEO commonly reports EAGAIN/WouldBlock. Under a
                    // scoped blocking socket this is the bounded send timeout.
                    let error = if self.operation_deadline.is_some() && is_receive_timeout(&error) {
                        std::io::Error::new(ErrorKind::TimedOut, error)
                    } else {
                        error
                    };
                    let context = crate::network::with_local_network_hint(
                        "sending GVCP command",
                        &error,
                        self.socket.peer_addr().ok().map(|peer| peer.ip()),
                    );
                    anyhow::Error::new(error).context(context)
                })?;
                Ok(sent)
            })?;
            first_send.get_or_insert(sent);
            let mut attempt_deadline = (Instant::now() + retry_wait).min(deadline);
            loop {
                let mut remaining = attempt_deadline.saturating_duration_since(Instant::now());
                if permit != Permit::Release
                    && let Some(lease) = &lease
                    && let Some(left) = lease.remaining()?
                {
                    remaining = remaining.min(left);
                }
                if remaining.is_zero() {
                    break;
                }
                self.socket.set_read_timeout(Some(remaining))?;
                match self.socket.recv(&mut buffer) {
                    Ok(length) => {
                        self.check_operation()?;
                        if permit != Permit::Release
                            && let Some(lease) = &lease
                        {
                            lease.check()?;
                        }
                        if length < 8 || be16(&buffer[..length], 6) != id {
                            continue;
                        }
                        let ack = parse_ack(&buffer[..length])?;
                        if ack.command != command + 1 && ack.command != PENDING_ACK {
                            continue;
                        }
                        if ack.status != 0 {
                            return Err(GvcpStatus(ack.status).into());
                        }
                        if ack.command == PENDING_ACK {
                            ensure!(
                                permit != Permit::Release,
                                "release did not complete synchronously"
                            );
                            ensure!(ack.payload.len() == 4, "invalid pending acknowledgement");
                            let completion = Duration::from_millis(u64::from(be16(ack.payload, 2)));
                            attempt_deadline =
                                (Instant::now() + completion + retry_wait).min(limit);
                            deadline = deadline.max(attempt_deadline);
                            continue;
                        }
                        return Ok(ControlReply {
                            payload: ack.payload.to_vec(),
                            first_send: first_send.unwrap_or(sent),
                        });
                    }
                    // Windows can end a socket wait a little before the timeout
                    // it was given; only the attempt deadline itself ends it, or
                    // an early wakeup would resend just before the cutoff.
                    Err(error)
                        if is_receive_timeout(&error) || error.kind() == ErrorKind::Interrupted => {
                    }
                    Err(error) => {
                        let context = crate::network::with_local_network_hint(
                            "receiving GVCP acknowledgement",
                            &error,
                            self.socket.peer_addr().ok().map(|peer| peer.ip()),
                        );
                        return Err(anyhow::Error::new(error).context(context));
                    }
                }
            }
            if attempt == 2 {
                break;
            }
        }
        if permit != Permit::Release
            && let Some(lease) = &lease
        {
            lease.check()?;
        }
        self.check_operation()?;
        Err(GvcpTimeout {
            command,
            waited: start.elapsed(),
        }
        .into())
    }
    fn read_register_owned(&mut self, address: u32) -> Result<(u32, u64)> {
        let reply = self.transact_owned(READ_REGISTER, &address.to_be_bytes(), Permit::Probe)?;
        ensure!(
            reply.payload.len() == 4,
            "invalid register read acknowledgement"
        );
        Ok((be32(&reply.payload, 0), reply.first_send))
    }
    fn probe_owner(&mut self) -> Result<()> {
        let Some(lease) = self.lease.clone() else {
            return Ok(());
        };
        let result = (|| -> Result<()> {
            let (privilege, sent) = self.read_register_owned(CONTROL_PRIVILEGE)?;
            if privilege != 2 {
                return Err(lease.retire("privilege_changed", format!("camera controller privilege changed to 0x{privilege:08x}; retiring this connection")));
            }
            if let Some(expected) = self.owner_endpoint {
                let (port, _) = self.read_register_owned(PRIMARY_PORT)?;
                let (ip, _) = self.read_register_owned(PRIMARY_IP)?;
                if port & 0xffff != u32::from(expected.port()) || ip != u32::from(*expected.ip()) {
                    return Err(lease.retire("owner_endpoint", "camera reports a different or absent controller endpoint; retiring this connection"));
                }
            }
            if self.disable_supported {
                let (configuration, _) = self.read_register_owned(GVCP_CONFIGURATION)?;
                if (configuration & HEARTBEAT_DISABLE != 0) != lease.disabled() {
                    return Err(lease.retire("heartbeat_state", "camera heartbeat-disable state changed unexpectedly; retiring this connection"));
                }
            }
            lease.renew(sent)
        })();
        if let Err(error) = result {
            // A caller cutoff does not contradict previously verified Disabled
            // ownership. The independent heartbeat continues its normal checks.
            let caller_expired = self
                .operation_deadline
                .is_some_and(|stop| Instant::now() >= stop);
            if caller_expired {
                lease.check()?;
                return Err(error);
            }
            if lease.disabled() && !ownership_retired(&error) {
                return Err(lease.retire(
                    "owner_unverified",
                    format!(
                        "disabled camera controller ownership could not be verified: {error:#}"
                    ),
                ));
            }
            return Err(error);
        }
        Ok(())
    }
    fn read_register(&mut self, address: u32) -> Result<u32> {
        ensure!(address.is_multiple_of(4), "unaligned GVCP register address");
        let ack = self.transact(READ_REGISTER, &address.to_be_bytes())?;
        ensure!(ack.len() == 4, "invalid register read acknowledgement");
        Ok(be32(&ack, 0))
    }
    fn prepare_managed(&mut self) -> Result<()> {
        if let Some(lease) = self.lease.clone()
            && lease.needs_probe(self.command_time())?
        {
            self.probe_owner()?;
        }
        Ok(())
    }
    fn write_raw(&mut self, address: u32, value: u32, permit: Permit) -> Result<()> {
        let mut payload = address.to_be_bytes().to_vec();
        payload.extend(value.to_be_bytes());
        let ack = self
            .transact_owned(WRITE_REGISTER, &payload, permit)?
            .payload;
        ensure!(ack.len() == 4, "invalid register write acknowledgement");
        ensure!(
            be32(&ack, 0) == 1,
            "camera did not acknowledge the register write"
        );
        Ok(())
    }
    fn write_register(&mut self, address: u32, value: u32) -> Result<()> {
        ensure!(address.is_multiple_of(4), "unaligned GVCP register address");
        let Some(lease) = self.lease.clone() else {
            return self.write_raw(address, value, Permit::Normal);
        };
        lease.check()?;
        match address {
            CONTROL_PRIVILEGE => match value {
                0 => self.final_release(),
                2 => {
                    self.prepare_managed()?;
                    self.write_raw(address, value, Permit::Managed)
                }
                _ => Err(Categorized::error(
                    crate::types::ErrorKind::Unsupported,
                    "changing controller mode or switchover/keyed privilege is unsupported on an owned connection",
                )),
            },
            HEARTBEAT_TIMEOUT => {
                if value == 0 {
                    return Err(Categorized::error(
                        crate::types::ErrorKind::Unsupported,
                        "a positive verified heartbeat timeout is required",
                    ));
                }
                if !lease.disabled() {
                    if lease.timeout_ms() != Some(value) {
                        return Err(Categorized::error(
                            crate::types::ErrorKind::Unsupported,
                            "changing a finite heartbeat timeout is unsupported; the actual camera timeout is retained",
                        ));
                    }
                    self.prepare_managed()?;
                    return self.write_raw(address, value, Permit::Managed);
                }
                self.prepare_managed()?;
                let write = self.write_raw(address, value, Permit::Managed);
                if let Err(error) = write {
                    lease.timer_changed(None)?;
                    return Err(error);
                }
                let actual = self.read_register_owned(address).map(|(value, _)| value);
                lease.timer_changed(actual.as_ref().ok().copied().filter(|value| *value != 0))?;
                let actual = actual?;
                ensure!(
                    actual != 0,
                    "camera returned a zero heartbeat timeout; finite re-enable permission was removed"
                );
                Ok(())
            }
            GVCP_CONFIGURATION => {
                let (old, _) = self.read_register_owned(address)?;
                let was_disabled = old & HEARTBEAT_DISABLE != 0;
                let disabling = value & HEARTBEAT_DISABLE != 0;
                if was_disabled != lease.disabled() {
                    return Err(lease.retire(
                        "heartbeat_state",
                        "camera heartbeat mode differs from the verified controller state",
                    ));
                }
                if disabling && !self.disable_supported {
                    return Err(Categorized::error(
                        crate::types::ErrorKind::Unsupported,
                        "camera does not advertise heartbeat-disable support",
                    ));
                }
                if was_disabled && !disabling {
                    lease.begin_enabling()?;
                }
                if !was_disabled && disabling {
                    self.probe_owner()?;
                }
                let result = (|| -> Result<()> {
                    self.write_raw(address, value, Permit::Managed)?;
                    let (actual, _) = self.read_register_owned(address)?;
                    if actual & HEARTBEAT_DISABLE != value & HEARTBEAT_DISABLE {
                        return Err(lease.retire(
                            "heartbeat_state_unverified",
                            "camera did not confirm the requested heartbeat mode",
                        ));
                    }
                    if !was_disabled && disabling {
                        lease.enter_disabled()?;
                    }
                    if was_disabled && !disabling {
                        lease.finish_enabling()?;
                        self.probe_owner()?;
                    }
                    Ok(())
                })();
                result.map_err(|error| {
                    if ownership_retired(&error) || was_disabled == disabling {
                        error
                    } else {
                        lease.retire(
                            "heartbeat_state_unverified",
                            format!("camera heartbeat mode could not be confirmed: {error:#}"),
                        )
                    }
                })
            }
            SWITCHOVER_KEY => Err(Categorized::error(
                crate::types::ErrorKind::Unsupported,
                "controller switchover keys are unsupported on an owned connection",
            )),
            _ => self.write_raw(address, value, Permit::Normal),
        }
    }
    fn final_release(&mut self) -> Result<()> {
        let Some(lease) = self.lease.clone() else {
            return self.write_raw(CONTROL_PRIVILEGE, 0, Permit::Release);
        };
        lease.check()?;
        self.prepare_managed()?;
        let restored = (|| -> Result<()> {
            if let Some(configuration) = self.restore_configuration.take() {
                let current = self.read_register(GVCP_CONFIGURATION)?;
                let restored = (current & !0x2) | (configuration & 0x2);
                if restored != current {
                    self.write_register(GVCP_CONFIGURATION, restored)?;
                }
            }
            Ok(())
        })();
        // No operation, restoration or retry follows this release request.
        let released = self
            .write_raw(CONTROL_PRIVILEGE, 0, Permit::Release)
            .context(
                "controller release was not confirmed; this connection sends no release retry",
            );
        restored?;
        released
    }
    fn release(&mut self, _heartbeat_timeout: u32) {
        let _ = self.final_release();
    }

    fn read_aligned(&mut self, address: u32, length: usize) -> Result<Vec<u8>> {
        ensure!(
            address.is_multiple_of(4) && length.is_multiple_of(4),
            "unaligned GVCP memory read"
        );
        if length == 4 && self.legacy_registers {
            return Ok(self.read_register(address)?.to_be_bytes().to_vec());
        }
        let mut result = Vec::with_capacity(length);
        for offset in (0..length).step_by(MEMORY_CHUNK) {
            let size = (length - offset).min(MEMORY_CHUNK);
            let current = address
                .checked_add(u32::try_from(offset).context("GVCP read offset exceeds u32")?)
                .context("GVCP address overflow")?;
            let mut request = [0u8; 8];
            request[..4].copy_from_slice(&current.to_be_bytes());
            request[4..].copy_from_slice(
                &u32::try_from(size)
                    .context("GVCP memory chunk exceeds u32")?
                    .to_be_bytes(),
            );
            let ack = self.transact(READ_MEMORY, &request)?;
            ensure!(
                ack.len() == 4 + size,
                "invalid memory read acknowledgement length"
            );
            ensure!(
                be32(&ack, 0) == current,
                "memory read acknowledged the wrong address"
            );
            result.extend_from_slice(&ack[4..]);
        }
        Ok(result)
    }

    fn write_aligned(&mut self, address: u32, data: &[u8]) -> Result<()> {
        ensure!(
            address.is_multiple_of(4) && data.len().is_multiple_of(4),
            "unaligned GVCP memory write"
        );
        let mut payload = Vec::new();
        for (index, bytes) in data.chunks(MEMORY_CHUNK).enumerate() {
            let current = address
                .checked_add(
                    u32::try_from(index * MEMORY_CHUNK).context("GVCP write offset exceeds u32")?,
                )
                .context("GVCP address overflow")?;
            // Register writes carry values, so camera-order data from a
            // little-endian device is flipped back.
            let flip = !self.legacy_registers && self.little_endian;
            let words = |control: &mut Self| -> Result<()> {
                for (word, value) in bytes.as_chunks::<4>().0.iter().enumerate() {
                    let value = if flip {
                        u32::from_le_bytes(*value)
                    } else {
                        u32::from_be_bytes(*value)
                    };
                    let offset = u32::try_from(word * 4)
                        .context("GVCP register write offset exceeds u32")?;
                    let address = current
                        .checked_add(offset)
                        .context("GVCP address overflow")?;
                    control.write_register(address, value)?;
                }
                Ok(())
            };
            if (bytes.len() == 4 && self.legacy_registers) || !self.write_memory {
                words(self)?;
                continue;
            }
            payload.clear();
            payload.reserve(4 + bytes.len());
            payload.extend_from_slice(&current.to_be_bytes());
            payload.extend_from_slice(bytes);
            match self.transact(WRITE_MEMORY, &payload) {
                // The acknowledgement carries a reserved half and a data index
                // whose unit varies between devices (simulators echo the
                // address), so only its presence is checked.
                Ok(ack) => ensure!(ack.len() >= 4, "invalid memory write acknowledgement"),
                // WRITE_MEMORY is optional in older GigE Vision devices.
                Err(error)
                    if error
                        .downcast_ref::<GvcpStatus>()
                        .is_some_and(|s| s.0 == 0x8001) =>
                {
                    words(self)?;
                }
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
    fn read_bytes(&mut self, address: u64, length: usize) -> Result<Vec<u8>> {
        let (base, size, skip) = memory_range(address, length)?;
        if size == 0 {
            return Ok(Vec::new());
        }
        let mut data = self.read_aligned(base, size)?;
        data.copy_within(skip..skip + length, 0);
        data.truncate(length);
        Ok(data)
    }
    fn write_bytes(&mut self, address: u64, data: &[u8]) -> Result<()> {
        let (base, size, skip) = memory_range(address, data.len())?;
        if size == 0 {
            return Ok(());
        }
        if self.lease.is_some() && managed_range(base, size) {
            if skip != 0 || data.len() != 4 || size != 4 {
                return Err(Categorized::error(
                    crate::types::ErrorKind::Unsupported,
                    "partial or mixed controller-register memory writes are unsupported",
                ));
            }
            if !self.legacy_registers && !self.register_order_known {
                return Err(Categorized::error(
                    crate::types::ErrorKind::Unsupported,
                    "controller-register memory byte order is unknown",
                ));
            }
            let bytes: [u8; 4] = data.try_into().context("incomplete managed register")?;
            let value = if !self.legacy_registers && self.little_endian {
                u32::from_le_bytes(bytes)
            } else {
                u32::from_be_bytes(bytes)
            };
            return self.write_register(base, value);
        }
        if skip == 0 && size == data.len() {
            return self.write_aligned(base, data);
        }
        // Preserve bytes surrounding partial-word writes; padding a write with
        // zeros would silently alter adjacent camera features.
        let mut aligned = self.read_aligned(base, size)?;
        aligned[skip..skip + data.len()].copy_from_slice(data);
        self.write_aligned(base, &aligned)
    }
}
fn memory_range(address: u64, length: usize) -> Result<(u32, usize, usize)> {
    ensure!(
        length <= MAX_PAYLOAD,
        "memory request exceeds 128 MiB safety limit"
    );
    let address = u32::try_from(address)
        .context("GigE Vision 1.x memory addresses are limited to 32 bits")?;
    if length == 0 {
        return Ok((address, 0, 0));
    }
    let address = u64::from(address);
    let end = address
        .checked_add(length as u64)
        .context("memory range overflow")?;
    ensure!(
        end <= 0x1_0000_0000,
        "GigE Vision memory range exceeds 32-bit address space"
    );
    let base = address & !3;
    let aligned_end = (end + 3) & !3;
    Ok((
        u32::try_from(base).context("aligned GVCP address exceeds u32")?,
        usize::try_from(aligned_end - base)
            .context("GVCP memory range exceeds host address size")?,
        usize::try_from(address - base)
            .context("GVCP alignment offset exceeds host address size")?,
    ))
}

/// Probe a known IPv4 camera without waiting for LAN-wide discovery.
pub(crate) fn probe(ip: Ipv4Addr, timeout: Duration) -> Result<CameraInfo> {
    ensure!(
        !ip.is_unspecified() && !ip.is_multicast() && ip != Ipv4Addr::BROADCAST,
        "expected a unicast camera IPv4 address"
    );
    let timeout = bounded_timeout(timeout);
    probe_identity(ip, timeout)
        .map_err(|error| crate::network::explain_probe_failure(ip, timeout, error))
}
fn probe_identity(ip: Ipv4Addr, timeout: Duration) -> Result<CameraInfo> {
    let mut control = Control::connect(SocketAddrV4::new(ip, GVCP_PORT), timeout / 2)?;
    if let Ok(payload) = control.transact(DISCOVERY, &[]) {
        return discovery_info(&payload, ip);
    }
    // Some cameras suppress unicast discovery while still exposing bootstrap
    // registers. Reading their identity does not acquire controller privilege.
    let identity = control
        .read_bytes(0, 0xf8)
        .context("camera did not answer discovery or bootstrap identity read")?;
    let mut info = discovery_info(&identity, ip)?;
    if info.id == "gige:000000000000" {
        info.id = format!("gige:{ip}");
    }
    if info.model.is_empty() {
        "GigE Vision camera".clone_into(&mut info.model);
    }
    Ok(info)
}

/// How long the camera at `info` keeps the control privilege of a
/// controller that went silent: its heartbeat timeout, which anyone may read.
pub(crate) fn heartbeat_timeout(info: &CameraInfo, timeout: Duration) -> Result<Duration> {
    let ip: Ipv4Addr = info
        .address
        .as_deref()
        .context("GigE camera has no address")?
        .parse()
        .context("invalid camera IPv4 address")?;
    let mut control = Control::connect(SocketAddrV4::new(ip, GVCP_PORT), timeout)?;
    Ok(Duration::from_millis(u64::from(
        control.read_register(HEARTBEAT_TIMEOUT)?,
    )))
}
/// Confirm a camera's unicast control path, without acquiring control or
/// changing any register. Broadcast discovery alone does not prove reachability.
/// All three reads share one deadline, including retries and pending ACKs.
pub(crate) fn probe_network(ip: Ipv4Addr, timeout: Duration) -> Result<Ipv4Addr> {
    ensure!(
        !ip.is_unspecified() && !ip.is_multicast() && ip != Ipv4Addr::BROADCAST,
        "expected a unicast camera IPv4 address"
    );
    probe_network_at(SocketAddrV4::new(ip, GVCP_PORT), timeout)
}
fn probe_network_at(address: SocketAddrV4, timeout: Duration) -> Result<Ipv4Addr> {
    let timeout = bounded_timeout(timeout);
    let deadline = Instant::now() + timeout;
    let mut control = Control::connect(address, timeout)?;
    for read in 1..=3 {
        let remaining = deadline.saturating_duration_since(Instant::now());
        ensure!(
            !remaining.is_zero(),
            "camera control path did not pass three reads before the deadline"
        );
        control.timeout = remaining;
        control
            .read_register(GVCP_CAPABILITY)
            .with_context(|| format!("camera control path failed check {read} of 3"))?;
    }
    match control.socket.local_addr()? {
        SocketAddr::V4(local) => Ok(*local.ip()),
        SocketAddr::V6(_) => bail!("camera control route is not IPv4"),
    }
}

/// How soon a heartbeat that went unanswered is sent again.
const HEARTBEAT_RETRY: Duration = Duration::from_millis(50);

struct Heartbeat {
    lease: Arc<Lease>,
    thread: Option<JoinHandle<()>>,
}
impl Heartbeat {
    fn start(control: Arc<Mutex<Control>>) -> Result<Self> {
        let lease = control
            .lock()
            .map_err(|_| anyhow!("GVCP mutex poisoned"))?
            .lease
            .clone()
            .context("controller lease was not initialized")?;
        let worker_lease = lease.clone();
        let thread = thread::Builder::new()
            .name("capturefab-gige-heartbeat".into())
            .spawn(move || {
                let (lock, wake) = &*worker_lease.wake;
                let mut wait = worker_lease.period();
                loop {
                    let state = lock
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let generation = state.generation;
                    let (state, _) = wake
                        .wait_timeout_while(state, wait, |state| {
                            !state.stop && state.generation == generation
                        })
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if state.stop {
                        break;
                    }
                    drop(state);
                    if worker_lease.check().is_err() {
                        break;
                    }
                    let result = control
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .probe_owner();
                    if worker_lease.check().is_err() {
                        break;
                    }
                    wait = if result.is_ok() {
                        worker_lease.period()
                    } else {
                        HEARTBEAT_RETRY.min(worker_lease.period())
                    };
                }
            })
            .context("starting GigE heartbeat thread")?;
        Ok(Self {
            lease,
            thread: Some(thread),
        })
    }
    fn check(&self) -> Result<()> {
        self.lease.check()
    }
    fn shutdown(&mut self) {
        let (lock, wake) = &*self.lease.wake;
        lock.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .stop = true;
        wake.notify_all();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
impl Drop for Heartbeat {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PacketSize {
    /// No probe yet, or an explicit optimization requested a fresh probe.
    Untested,
    /// Probes failed; stream startup uses the conservative default.
    Unverified,
    /// Recheck this size on startup before searching for another one.
    Verified(u32),
}

struct GigE {
    control: Arc<Mutex<Control>>,
    heartbeat: Heartbeat,
    camera_ip: Ipv4Addr,
    host_ip: Ipv4Addr,
    timestamp_frequency: u64,
    heartbeat_timeout: u32,
    resend: bool,
    big_endian_mono16: bool,
    multipart_projection: bool,
    /// Vendor layouts qualified from real identity and static XML before streaming.
    pixel_encodings: Vec<crate::types::PixelEncoding>,
    stream_source: OnceCell<Option<SocketAddr>>,
    packet_size: PacketSize,
    stats: TransportStats,
    stream: Option<Stream>,
    xml_cache: Option<String>,
    /// The key of the XML read from the disk cache, while it may still turn
    /// out wrong.
    xml_key: Option<String>,
}

/// Acquire camera control. An independent heartbeat keeps the connection alive
/// even while the camera is idle or waiting for an external trigger.
pub(crate) fn open(info: &CameraInfo, timeout: Duration) -> Result<Box<dyn Backend>> {
    let ip: Ipv4Addr = info
        .address
        .as_deref()
        .context("GigE camera has no address")?
        .parse()
        .context("invalid camera IPv4 address")?;
    let mut camera = connect(SocketAddrV4::new(ip, GVCP_PORT), timeout)?;
    camera.big_endian_mono16 = big_endian_mono16(&info.vendor, &info.model);
    Ok(Box::new(camera))
}
/// First-generation Point Grey Blackfly `GigE` models send Mono16 big-endian
/// even with `GevSCPSBigEndian` clear (Aravis keeps the same list).
fn big_endian_mono16(vendor: &str, model: &str) -> bool {
    vendor == "Point Grey Research"
        && ["Blackfly BFLY-PGE-31S4M", "Blackfly BFLY-PGE-14S2C"].contains(&model)
}
fn connect(address: SocketAddrV4, timeout: Duration) -> Result<GigE> {
    connect_control(Control::connect(address, timeout)?)
}
fn connect_control(mut control: Control) -> Result<GigE> {
    let address = match control.socket.peer_addr()? {
        SocketAddr::V4(address) => address,
        SocketAddr::V6(_) => bail!("camera control peer is not IPv4"),
    };
    let host_ip = match control.socket.local_addr()? {
        SocketAddr::V4(address) => *address.ip(),
        SocketAddr::V6(_) => bail!("camera control route is not IPv4"),
    };
    if control.read_register(CONTROL_PRIVILEGE)? != 0 {
        return Err(Categorized::new(
            crate::types::ErrorKind::CameraBusy,
            "camera is controlled by another application; disconnect it first",
        )
        .into());
    }
    let heartbeat_timeout = control.read_register(HEARTBEAT_TIMEOUT)?;
    ensure!(
        heartbeat_timeout != 0,
        "camera heartbeat timeout is zero; no finite controller lease can be verified"
    );
    let capability = control.read_register(GVCP_CAPABILITY).ok();
    control.disable_supported = capability.is_some_and(|bits| bits & DISABLE_CAPABILITY != 0);
    let disabled = if control.disable_supported {
        control.read_register(GVCP_CONFIGURATION)? & HEARTBEAT_DISABLE != 0
    } else {
        false
    };
    if capability.is_some_and(|bits| bits & OWNER_ENDPOINT != 0) {
        control.owner_endpoint = match control.socket.local_addr()? {
            SocketAddr::V4(address) => Some(address),
            SocketAddr::V6(_) => None,
        };
    }
    let lease = Lease::claiming(heartbeat_timeout, disabled)?;
    #[cfg(test)]
    let lease = control.lease_clock.as_ref().map_or(lease, |clock| {
        Lease::claiming_for_test(heartbeat_timeout, disabled, clock.clone())
    });
    control.lease = Some(lease);
    if let Err(error) = control.write_raw(CONTROL_PRIVILEGE, 2, Permit::Claim) {
        let _ = control.lease.as_ref().expect("claim lease").retire(
            "claim_unverified",
            format!("camera controller acquisition was not confirmed: {error:#}"),
        );
        return Err(
            error.context("acquiring camera controller privilege; no unverified release was sent")
        );
    }
    // Setup cleanup releases only while this acquisition still has proof.
    let initialized = (|| -> Result<(u32, Duration, u64, bool)> {
        control.probe_owner()?;
        let lease = control.lease.as_ref().expect("claim lease").clone();
        if disabled {
            lease.finish_claim()?;
        }
        let initial_readback = control
            .read_register_owned(HEARTBEAT_TIMEOUT)
            .map(|(value, _)| value);
        if disabled {
            lease.timer_changed(
                initial_readback
                    .as_ref()
                    .ok()
                    .copied()
                    .filter(|value| *value != 0),
            )?;
            let value = initial_readback?;
            ensure!(
                value != 0,
                "camera returned a zero heartbeat timeout after acquisition"
            );
        } else {
            let value = initial_readback.map_err(|error| {
                lease.retire(
                    "initial_timeout_unverified",
                    format!(
                        "camera heartbeat timeout readback failed during acquisition: {error:#}"
                    ),
                )
            })?;
            if value != heartbeat_timeout {
                return Err(lease.retire("initial_timeout_changed", "camera heartbeat timeout changed during acquisition; this ownership epoch cannot be verified"));
            }
            lease.finish_claim()?;
        }
        let device_mode = control.read_register(DEVICE_MODE);
        control.register_order_known = device_mode.is_ok();
        control.little_endian = device_mode.is_ok_and(|mode| mode & 0x8000_0000 == 0);
        let resend = capability.is_some_and(|bits| bits & 0x4 != 0);
        control.write_memory = capability.is_none_or(|bits| bits & 0x2 != 0);
        let response = match control.read_register(PENDING_TIMEOUT) {
            Ok(ms @ 1..=5000) => Duration::from_millis(u64::from(ms)),
            _ => Duration::ZERO,
        };
        if capability.is_some_and(|bits| bits & 0x20 != 0)
            && let Ok(configuration) = control.read_register(GVCP_CONFIGURATION)
            && configuration & 0x2 == 0
            && control
                .write_register(GVCP_CONFIGURATION, configuration | 0x2)
                .is_ok()
        {
            control.restore_configuration = Some(configuration);
        }
        control.response = response;
        control.pending_limit = Some(Duration::from_millis(u64::from(heartbeat_timeout) * 2 / 3));
        let timestamp_frequency = match (
            control.read_register(TIMESTAMP_HIGH),
            control.read_register(TIMESTAMP_LOW),
        ) {
            (Ok(high), Ok(low)) => (u64::from(high) << 32) | u64::from(low),
            _ => 0,
        };
        control.lease.as_ref().expect("claim lease").check()?;
        Ok((
            heartbeat_timeout,
            Duration::from_millis(u64::from(heartbeat_timeout)),
            timestamp_frequency,
            resend,
        ))
    })();
    let (heartbeat_timeout, _lost_after, timestamp_frequency, resend) = match initialized {
        Ok(values) => values,
        Err(error) => {
            control.release(heartbeat_timeout);
            return Err(error);
        }
    };
    let control = Arc::new(Mutex::new(control));
    let heartbeat = match Heartbeat::start(control.clone()) {
        Ok(heartbeat) => heartbeat,
        Err(error) => {
            if let Ok(mut control) = control.lock() {
                control.release(heartbeat_timeout);
            }
            return Err(error);
        }
    };
    Ok(GigE {
        control,
        heartbeat,
        camera_ip: *address.ip(),
        host_ip,
        timestamp_frequency,
        heartbeat_timeout,
        resend,
        big_endian_mono16: false,
        multipart_projection: false,
        pixel_encodings: Vec::new(),
        stream_source: OnceCell::new(),
        packet_size: PacketSize::Untested,
        stats: TransportStats::default(),
        stream: None,
        xml_cache: None,
        xml_key: None,
    })
}

impl GigE {
    fn control_until<T>(
        &self,
        deadline: Instant,
        action: impl FnOnce(&mut Control) -> Result<T>,
    ) -> Result<T> {
        self.heartbeat.check()?;
        let mut control = loop {
            control_remaining(deadline)?;
            match self.control.try_lock() {
                Ok(control) => break control,
                Err(std::sync::TryLockError::Poisoned(_)) => bail!("GVCP mutex poisoned"),
                Err(std::sync::TryLockError::WouldBlock) => {
                    thread::sleep(control_remaining(deadline)?.min(Duration::from_millis(1)));
                }
            }
        };
        self.heartbeat.check()?;
        control_remaining(deadline)?;
        let previous = control.operation_deadline.replace(deadline);
        let result = action(&mut control);
        control.operation_deadline = previous;
        match result {
            Ok(value) => {
                control_remaining(deadline)?;
                Ok(value)
            }
            Err(error) => Err(error),
        }
    }
}
impl RegisterIo for GigE {
    fn read_memory_until(
        &mut self,
        address: u64,
        length: usize,
        deadline: Instant,
    ) -> Result<Vec<u8>> {
        self.control_until(deadline, |control| control.read_bytes(address, length))
    }
    fn write_memory_until(&mut self, address: u64, data: &[u8], deadline: Instant) -> Result<()> {
        self.control_until(deadline, |control| control.write_bytes(address, data))
    }
    fn read_memory(&mut self, address: u64, length: usize) -> Result<Vec<u8>> {
        self.heartbeat.check()?;
        self.control
            .lock()
            .map_err(|_| anyhow!("GVCP mutex poisoned"))?
            .read_bytes(address, length)
    }
    fn write_memory(&mut self, address: u64, data: &[u8]) -> Result<()> {
        self.heartbeat.check()?;
        self.control
            .lock()
            .map_err(|_| anyhow!("GVCP mutex poisoned"))?
            .write_bytes(address, data)
    }
}
impl Backend for GigE {
    fn configure_pixel_encodings(
        &mut self,
        encodings: &[crate::types::PixelEncoding],
    ) -> Result<()> {
        ensure!(
            self.stream.is_none() && encodings.len() <= crate::types::LucidPixelFormat::ALL.len(),
            "invalid GigE pixel encoding configuration"
        );
        for &encoding in encodings {
            ensure!(
                matches!(encoding, crate::types::PixelEncoding::Lucid { .. })
                    && encoding.namespace().is_none(),
                Categorized::error(
                    crate::types::ErrorKind::Unsupported,
                    "native GigE accepts only qualified LUCID vendor layouts"
                )
            );
            encoding.check(encoding.code())?;
        }
        self.pixel_encodings = encodings.to_vec();
        Ok(())
    }
    fn enable_multipart_projection(&mut self) -> Result<()> {
        self.heartbeat.check()?;
        self.multipart_projection = true;
        if let Some(stream) = self.stream.as_mut() {
            stream.assembler.multipart_projection = true;
        }
        Ok(())
    }
    fn xml(&mut self) -> Result<String> {
        if let Some(xml) = &self.xml_cache {
            return Ok(xml.clone());
        }
        let mut errors = Vec::new();
        let mut first_typed_failure = None;
        for url_address in [0x200, 0x400] {
            let result = (|| -> Result<String> {
                let url = fixed_text(&self.read_memory(url_address, 512)?);
                if !url
                    .split_once(':')
                    .is_some_and(|(scheme, _)| scheme.eq_ignore_ascii_case("local"))
                {
                    let timeout = self
                        .control
                        .lock()
                        .map_err(|_| anyhow!("GVCP mutex poisoned"))?
                        .timeout;
                    let (name, data) = fetch_xml_http(&url, timeout)?;
                    let identity = self.read_memory(0x48, 0x60)?;
                    // HTTP has no camera-memory tail to cheaply validate a cache
                    // entry. Download afresh and key the decoded cache by all
                    // fetched bytes, so a changed vendor file cannot stay stale.
                    let key = xml_cache::key(&[url.as_bytes(), &identity, &data]);
                    self.xml_key = Some(key.clone());
                    if let Some(xml) = xml_cache::load(&key) {
                        return Ok(xml);
                    }
                    let xml = decode_xml(&name, data)?;
                    xml_cache::store(&key, &xml);
                    return Ok(xml);
                }
                let local = parse_local_url(&url)?;
                // The URL (a SHA-1 in its query where the device gives one),
                // the vendor, model and device version, and the end of the
                // file name the XML on disk.
                let identity = self.read_memory(0x48, 0x60)?;
                let tail = xml_cache::TAIL.min(local.length);
                let end = self.read_memory(local.address + (local.length - tail) as u64, tail)?;
                let key = xml_cache::key(&[url.as_bytes(), &identity, &end]);
                if let Some(xml) = xml_cache::load(&key) {
                    self.xml_key = Some(key);
                    return Ok(xml);
                }
                let data = self.read_memory(local.address, local.length)?;
                let xml = decode_xml(&local.name, data)?;
                xml_cache::store(&key, &xml);
                Ok(xml)
            })();
            match result {
                Ok(xml) => {
                    self.control
                        .lock()
                        .map_err(|_| anyhow!("GVCP mutex poisoned"))?
                        .legacy_registers = legacy_registers(&xml);
                    self.xml_cache = Some(xml.clone());
                    return Ok(xml);
                }
                Err(error) => {
                    errors.push(format!(
                        "URL at 0x{url_address:x}: {}",
                        xml_diagnostic(&error)
                    ));
                    if first_typed_failure.is_none()
                        && crate::types::classify(&error).kind
                            != crate::types::ErrorKind::OperationFailed
                    {
                        first_typed_failure = Some(error);
                    }
                }
            }
        }
        let message = format!("unable to load camera GenICam XML: {}", errors.join("; "));
        if let Some(error) = first_typed_failure {
            return Err(error.context(message));
        }
        bail!("{message}")
    }
    fn forget_xml(&mut self) -> bool {
        self.xml_cache = None;
        self.xml_key
            .take()
            .inspect(|key| xml_cache::forget(key))
            .is_some()
    }
    #[deny(clippy::significant_drop_tightening)]
    fn start(&mut self, payload_size: usize) -> Result<()> {
        self.heartbeat.check()?;
        ensure!(self.stream.is_none(), "GigE receiving is already started");
        ensure!(
            payload_size > 0 && payload_size <= MAX_PAYLOAD,
            "camera payload size must be 1..=128 MiB"
        );
        let socket = UdpSocket::bind((self.host_ip, 0)).context("opening GVSP receive socket")?;
        let receive = socket2::SockRef::from(&socket);
        request_receive_buffer(payload_size, |size| set_receive_buffer(&socket, size));
        let receive_buffer = receive.recv_buffer_size().ok();
        let port = socket.local_addr()?.port();
        let camera = IpAddr::V4(self.camera_ip);
        let mtu = host_mtu(self.host_ip);
        let cap = mtu.map_or(PACKET_SIZE_MAX, |mtu| mtu.min(PACKET_SIZE_MAX)) & !3;
        let mut buffer = vec![0; 65_536];
        let mut control = self
            .control
            .lock()
            .map_err(|_| anyhow!("GVCP mutex poisoned"))?;
        let gvcp = control.socket.peer_addr()?;
        let mut source = None;
        let result = (|| -> Result<(u32, bool, bool)> {
            control.write_register(STREAM_ADDRESS, u32::from(self.host_ip))?;
            control.write_register(STREAM_PORT, u32::from(port))?;
            source = *self
                .stream_source
                .get_or_init(|| stream_source(&mut control, self.camera_ip));
            self.heartbeat.check()?;
            traverse(&socket, source);
            let original = control.read_register(STREAM_PACKET_SIZE)?;
            let mut test =
                |size| probe_packet_size(&mut control, &socket, &mut buffer, camera, size);
            let negotiated = match self.packet_size {
                PacketSize::Unverified => None,
                PacketSize::Verified(size) => {
                    test(size).or_else(|| ladder(&mut test, cap, mtu.is_some()))
                }
                PacketSize::Untested => ladder(&mut test, cap, mtu.is_some()),
            };
            self.packet_size = negotiated.map_or(PacketSize::Unverified, PacketSize::Verified);
            // Pixels are decoded little-endian, the GigE Vision default.
            control.write_register(
                STREAM_PACKET_SIZE,
                (original & SCPS_NO_FRAGMENT) | negotiated.unwrap_or_else(|| cap.min(1500)),
            )?;
            let scps = control.read_register(STREAM_PACKET_SIZE)?;
            let packet_size = scps & 0xffff;
            ensure!(
                (576..=PACKET_SIZE_MAX).contains(&packet_size),
                "camera selected unsupported stream packet size {packet_size}"
            );
            drain(&socket, &mut buffer);
            socket.set_read_timeout(Some(POLL))?;
            Ok((
                packet_size,
                negotiated.is_some(),
                scps & SCPS_BIG_ENDIAN != 0,
            ))
        })();
        let (packet_size, negotiated, big_endian) = match result {
            Ok(settings) => settings,
            Err(error) => {
                let _ = control.write_register(STREAM_PORT, 0);
                return Err(error);
            }
        };
        drop(control);
        let mut notes: Vec<String> = receive_buffer
            .and_then(|bytes| receive_buffer_note(bytes, payload_size))
            .into_iter()
            .collect();
        if !negotiated {
            notes.push(format!(
                "camera did not pass repeated stream packet tests; if frames time out, allow inbound UDP from {}",
                self.camera_ip
            ));
        }
        if big_endian {
            notes.push(
                "camera kept GevSCPSBigEndian set; 16- and 32-bit samples are swapped to little-endian"
                    .into(),
            );
        }
        self.heartbeat.check()?;
        let mut assembler =
            Assembler::new(payload_size, packet_size as usize, self.timestamp_frequency);
        assembler.resend = self.resend;
        assembler.multipart_projection = self.multipart_projection;
        assembler.big_endian = big_endian;
        assembler.big_endian_mono16 = self.big_endian_mono16;
        assembler.pixel_encodings.clone_from(&self.pixel_encodings);
        assembler.stats = TransportStats {
            packet_size: Some(packet_size),
            receive_buffer_bytes: receive_buffer.map(|bytes| bytes as u64),
            notes,
            ..std::mem::take(&mut self.stats)
        };
        self.stream = Some(Stream {
            socket,
            gvcp,
            source,
            buffer,
            header: [0; 20],
            overflow: vec![0; 64 * 1024],
            scratch: Vec::new(),
            scatter: scatter(std::env::var_os(SCATTER_ENV).as_deref()),
            #[cfg(any(test, target_os = "linux"))]
            batch: cfg!(target_os = "linux").then(|| {
                batch::Batch::new((packet_size as usize).max(multipart::MAX_METADATA_DATAGRAM))
            }),
            #[cfg(target_os = "linux")]
            predicted: Vec::new(),
            timeout: POLL,
            next_poll: Instant::now(),
            next_traversal: Instant::now() + TRAVERSAL,
            request_id: 0,
            assembler,
            lease: Some(self.heartbeat.lease.clone()),
        });
        Ok(())
    }
    #[deny(clippy::significant_drop_tightening)]
    fn optimize_network(&mut self, payload_size: usize) -> Result<crate::network::NetworkTuning> {
        self.heartbeat.check()?;
        ensure!(
            self.stream.is_none(),
            "stop GigE receiving before optimizing the network"
        );
        ensure!(
            payload_size > 0 && payload_size <= MAX_PAYLOAD,
            "camera payload size must be 1..=128 MiB"
        );
        self.heartbeat.check()?;
        // Both successful and unsuccessful prior negotiations must be retried
        // after the user changes the cable, interface or firewall configuration.
        self.packet_size = PacketSize::Untested;
        let socket =
            UdpSocket::bind((self.host_ip, 0)).context("opening GVSP network test socket")?;
        let receive = socket2::SockRef::from(&socket);
        request_receive_buffer(payload_size, |size| set_receive_buffer(&socket, size));
        let receive_buffer = receive.recv_buffer_size().ok();
        let port = socket.local_addr()?.port();
        let mtu = host_mtu(self.host_ip);
        let cap = mtu.unwrap_or(PACKET_SIZE_MAX).min(PACKET_SIZE_MAX) & !3;
        ensure!(
            cap >= 576,
            "network interface MTU is too small for GigE Vision streaming"
        );
        let mut buffer = vec![0; 65_536];
        let mut control = self
            .control
            .lock()
            .map_err(|_| anyhow!("GVCP mutex poisoned"))?;
        let saved_timeout = control.timeout;
        // A network scan must not spend the normal, potentially long camera
        // timeout on each rejected test size.
        control.timeout = saved_timeout.min(Duration::from_millis(200));
        let result = (|| -> Result<(u32, Option<u32>)> {
            let address = control.read_register(STREAM_ADDRESS)?;
            let stream_port = control.read_register(STREAM_PORT)?;
            let original = control.read_register(STREAM_PACKET_SIZE)?;
            let tuned = (|| -> Result<(u32, Option<u32>)> {
                control.write_register(STREAM_ADDRESS, u32::from(self.host_ip))?;
                control.write_register(STREAM_PORT, u32::from(port))?;
                let source = self
                    .stream_source
                    .get_or_init(|| stream_source(&mut control, self.camera_ip));
                self.heartbeat.check()?;
                traverse(&socket, *source);
                let negotiated = ladder(
                    |size| {
                        probe_packet_size(
                            &mut control,
                            &socket,
                            &mut buffer,
                            IpAddr::V4(self.camera_ip),
                            size,
                        )
                    },
                    cap,
                    mtu.is_some(),
                );
                let selected = negotiated.unwrap_or_else(|| cap.min(1500));
                control
                    .write_register(STREAM_PACKET_SIZE, (original & SCPS_NO_FRAGMENT) | selected)?;
                let actual = control.read_register(STREAM_PACKET_SIZE)? & 0xffff;
                ensure!(
                    (576..=selected).contains(&actual)
                        && (negotiated.is_none() || actual == selected),
                    "camera did not retain the selected {selected}-byte packet size (read back {actual})"
                );
                Ok((actual, negotiated))
            })();
            // A temporary test destination must never remain in the camera,
            // even when a command, packet test or settings readback fails.
            let restore_address = control.write_register(STREAM_ADDRESS, address);
            let restore_port = control.write_register(STREAM_PORT, stream_port);
            if tuned.is_err() || restore_address.is_err() || restore_port.is_err() {
                let restore_packet =
                    control.write_register(STREAM_PACKET_SIZE, original & !SCPS_FIRE);
                restore_address.context("restoring stream address after network test")?;
                restore_port.context("restoring stream port after network test")?;
                restore_packet.context("restoring packet size after failed network test")?;
            }
            tuned
        })();
        control.timeout = saved_timeout;
        drop(control);
        let (packet_size, negotiated) = result?;
        self.packet_size = negotiated.map_or(PacketSize::Unverified, PacketSize::Verified);
        let mut warnings = Vec::new();
        if negotiated.is_none() {
            warnings.push(format!("Stream delivery is unverified: the camera did not deliver three consecutive test packets. Applied a conservative {packet_size}-byte packet size; check inbound UDP, the cable and the camera's network path."));
        }
        if let Some(note) =
            receive_buffer.and_then(|bytes| receive_buffer_note(bytes, payload_size))
        {
            warnings.push(note);
        }
        self.stats.packet_size = Some(packet_size);
        self.stats.receive_buffer_bytes = receive_buffer.map(|bytes| bytes as u64);
        self.stats.notes.clone_from(&warnings);
        let packet_change = if negotiated.is_some() {
            format!(
                "Stream packet size: {packet_size} bytes, verified with three consecutive test packets"
            )
        } else {
            format!("Stream packet size: {packet_size} bytes (conservative default)")
        };
        let mut changes = vec![packet_change];
        if let Some(bytes) = receive_buffer {
            changes.push(format!("Receive buffer: {bytes} bytes; this buffer policy will also be applied when acquisition resumes"));
        }
        Ok(crate::network::NetworkTuning {
            packet_size: Some(packet_size),
            throughput_limit_bps: None,
            changes,
            warnings,
        })
    }
    fn next_frame(&mut self, timeout: Duration) -> Result<Frame> {
        self.heartbeat.check()?;
        let camera = IpAddr::V4(self.camera_ip);
        let stream = self
            .stream
            .as_mut()
            .context("GigE receiving has not been started")?;
        let deadline = Instant::now() + bounded_timeout(timeout);
        let mut last_error = None;
        let mut now = Instant::now();
        if now > stream.next_poll + POLL {
            for frame in &mut stream.assembler.frames {
                frame.last_packet = now;
                frame.requested_at = now;
            }
        }
        loop {
            if let Some(frame) = stream.assembler.ready.pop_front() {
                self.heartbeat.check()?;
                return Ok(frame);
            }
            if now >= stream.next_poll {
                self.heartbeat.check()?;
                stream.assembler.poll(now);
                stream.flush();
                if now >= stream.next_traversal {
                    self.heartbeat.check()?;
                    traverse(&stream.socket, stream.source);
                    stream.next_traversal = now + TRAVERSAL;
                }
                stream.next_poll = now + POLL;
                continue;
            }
            let remaining = deadline.saturating_duration_since(now);
            if remaining.is_zero() {
                break;
            }
            let wait = stream.wait(remaining, now);
            if wait != stream.timeout {
                stream.socket.set_read_timeout(Some(wait))?;
                stream.timeout = wait;
            }
            let received = stream.receive(camera);
            now = Instant::now();
            match received {
                Ok(Some(result)) => {
                    stream.flush();
                    match result {
                        Ok(Some(frame)) => {
                            self.heartbeat.check()?;
                            return Ok(frame);
                        }
                        Ok(None) => {}
                        Err(error)
                            if error.downcast_ref::<Categorized>().is_some_and(|error| {
                                error.kind == crate::types::ErrorKind::Unsupported
                            }) =>
                        {
                            return Err(error);
                        }
                        Err(error) => last_error = Some(error.to_string()),
                    }
                }
                Ok(None) => {}
                Err(error)
                    if is_receive_timeout(&error)
                        || matches!(
                            error.kind(),
                            ErrorKind::Interrupted | ErrorKind::ConnectionReset
                        ) => {}
                Err(error) => return Err(error).context("receiving GVSP frame"),
            }
        }
        let incomplete = stream.assembler.incomplete_summary();
        bail!(
            "frame receive timed out; {incomplete}{}",
            last_error
                .map(|e| format!("; last packet error: {e}"))
                .unwrap_or_default()
        )
    }
    fn stop(&mut self) -> Result<()> {
        if let Some(stream) = self.stream.take() {
            self.stats = stream.assembler.stats;
            self.control
                .lock()
                .map_err(|_| anyhow!("GVCP mutex poisoned"))?
                .write_register(STREAM_PORT, 0)?;
        }
        Ok(())
    }
    fn stop_after_frame_error(&self) -> bool {
        self.heartbeat.lease.is_retired()
    }
    fn terminal_error(&self) -> Option<anyhow::Error> {
        self.heartbeat.lease.check().err()
    }
    fn stats(&self) -> Option<TransportStats> {
        Some(
            self.stream
                .as_ref()
                .map_or(&self.stats, |stream| &stream.assembler.stats)
                .clone(),
        )
    }
    fn buffered_frames(&mut self) -> u32 {
        if self.heartbeat.check().is_err() {
            return 0;
        }
        let camera = IpAddr::V4(self.camera_ip);
        self.stream
            .as_mut()
            .map_or(0, |stream| stream.take_in(camera))
    }
    #[deny(clippy::significant_drop_tightening)]
    fn device_time_ns(&mut self) -> Option<u64> {
        if self.timestamp_frequency == 0 || self.stream.is_none() {
            return None;
        }
        let mut control = self.control.lock().ok()?;
        control
            .write_register(TIMESTAMP_CONTROL, TIMESTAMP_LATCH)
            .ok()?;
        let high = control.read_register(TIMESTAMP_VALUE_HIGH).ok()?;
        let low = control.read_register(TIMESTAMP_VALUE_LOW).ok()?;
        drop(control);
        Some(ticks_ns(
            (u64::from(high) << 32) | u64::from(low),
            self.timestamp_frequency,
        ))
    }
}
impl Drop for GigE {
    fn drop(&mut self) {
        self.heartbeat.shutdown();
        let _ = self.stop();
        if let Ok(mut control) = self.control.lock() {
            control.release(self.heartbeat_timeout);
        }
    }
}

/// Ask for a receive buffer of `size` bytes. On Linux, where a plain
/// request is silently cut to `net.core.rmem_max` (often about 200 KiB,
/// far less than a frame), a process allowed to (`CAP_NET_ADMIN`) gets the
/// size it asks for, within a sixteenth of the memory left; one that is
/// not gets what the limit allows.
fn set_receive_buffer(socket: &UdpSocket, size: usize) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd as _;
        let most = crate::host::memory_budget().map_or(usize::MAX, |budget| {
            usize::try_from(budget / 16).unwrap_or(usize::MAX)
        });
        let forced = libc::c_int::try_from(size.min(most)).unwrap_or(libc::c_int::MAX);
        let length =
            libc::socklen_t::try_from(size_of::<libc::c_int>()).map_err(std::io::Error::other)?;
        // SAFETY: setsockopt reads the int it is given, for this socket.
        let set = unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_RCVBUFFORCE,
                (&raw const forced).cast(),
                length,
            )
        };
        if set == 0 {
            return Ok(());
        }
    }
    socket2::SockRef::from(socket).set_recv_buffer_size(size)
}
fn request_receive_buffer(payload: usize, mut set: impl FnMut(usize) -> std::io::Result<()>) {
    let mut size = payload.saturating_mul(8).clamp(4 << 20, 64 << 20);
    while set(size).is_err() && size > 256 << 10 {
        size /= 2;
    }
}
fn receive_buffer_note(bytes: usize, payload: usize) -> Option<String> {
    let linux = cfg!(target_os = "linux");
    let usable = if linux { bytes / 2 } else { bytes };
    (usable < payload.saturating_mul(2)).then(|| {
        let hint = if linux {
            "; raise net.core.rmem_max, e.g. sysctl -w net.core.rmem_max=33554432"
        } else if cfg!(target_os = "macos") {
            "; raise kern.ipc.maxsockbuf"
        } else {
            ""
        };
        format!("receive buffer of {usable} bytes holds less than two {payload}-byte frames{hint}")
    })
}
fn host_mtu(host: Ipv4Addr) -> Option<u32> {
    crate::network::interfaces()
        .ok()?
        .into_iter()
        .find(|interface| interface.address == host)
        .and_then(|interface| interface.mtu)
}
/// The camera's stream source address, when it reports one (SCSP).
fn stream_source(control: &mut Control, camera: Ipv4Addr) -> Option<SocketAddr> {
    if control.read_register(STREAM_CAPABILITY).ok()? & 0x8000_0000 == 0 {
        return None;
    }
    let [_, _, high, low] = control
        .read_register(STREAM_SOURCE_PORT)
        .ok()?
        .to_be_bytes();
    let port = u16::from_be_bytes([high, low]);
    (port != 0).then(|| SocketAddr::from((camera, port)))
}
/// A datagram from the receive socket to the camera's stream source port
/// lets a stateful host firewall pass the stream as its replies; repeated
/// while receiving, it keeps that path open through long trigger gaps.
fn traverse(socket: &UdpSocket, source: Option<SocketAddr>) {
    if let Some(source) = source {
        let _ = socket.send_to(&[0; 4], source);
    }
}
fn drain(socket: &UdpSocket, buffer: &mut [u8]) {
    if socket.set_nonblocking(true).is_ok() {
        for _ in 0..256 {
            if socket.recv(buffer).is_err() {
                break;
            }
        }
        let _ = socket.set_nonblocking(false);
    }
}
fn probe_packet_size(
    control: &mut Control,
    socket: &UdpSocket,
    buffer: &mut [u8],
    camera: IpAddr,
    size: u32,
) -> Option<u32> {
    let control_peer = control.socket.peer_addr().ok()?;
    let mut readback = None;
    for _ in 0..3 {
        drain(socket, buffer);
        let value = SCPS_FIRE | SCPS_NO_FRAGMENT | size;
        control.write_register(STREAM_PACKET_SIZE, value).ok()?;
        let tested = control.read_register(STREAM_PACKET_SIZE).ok()? & 0xffff;
        if !(576..=size).contains(&tested) || readback.is_some_and(|previous| previous != tested) {
            return None;
        }
        readback = Some(tested);
        // Stream-path validation has the same configured latency budget as
        // control traffic; a fixed short deadline mistakes host jitter for a
        // smaller path MTU. All three packets must still pass independently.
        let deadline = Instant::now() + control.timeout;
        let mut received = false;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() || socket.set_read_timeout(Some(remaining)).is_err() {
                break;
            }
            match socket.recv_from(buffer) {
                Ok((length, source))
                    if source.ip() == camera
                        && source != control_peer
                        && length + 28 == tested as usize =>
                {
                    received = true;
                    break;
                }
                Err(error) if is_receive_timeout(&error) => break,
                _ => {}
            }
        }
        if !received {
            return None;
        }
    }
    readback
}
fn ladder(mut test: impl FnMut(u32) -> Option<u32>, cap: u32, mtu_known: bool) -> Option<u32> {
    if cap > 1500
        && let Some(size) = test(cap)
    {
        return Some(size);
    }
    let standard = cap.min(1500);
    let (mut low, mut high) = match test(standard) {
        Some(size) if mtu_known && cap > 1500 => (size, cap),
        Some(size) => return Some(size),
        None => (test(576)?, standard),
    };
    for _ in 0..6 {
        let middle = low.midpoint(high) & !3;
        if middle <= low {
            break;
        }
        match test(middle) {
            Some(size) => low = size,
            None => high = middle,
        }
    }
    Some(low)
}

/// Devices whose XML claims schema 1.1 or newer while their registers follow
/// the schema 1.0 `GigE` Vision convention (the list Aravis keeps).
const LEGACY_REGISTER_DEVICES: [(&str, &str); 19] = [
    ("AT_Automation_Technology_GmbH", "C6_X_GigE"),
    ("DO3THINK", "MGV518"),
    ("EVK", "HELIOS"),
    ("IDS_Imaging_Development_Systems_GmbH", "GV_53FxLE_M"),
    ("IDS_Imaging_Development_Systems_GmbH", "GV_58CxLE_M"),
    ("IDS_Imaging_Development_Systems_GmbH", "GV_524xCP_NIR"),
    ("IDS_Imaging_Development_Systems_GmbH", "GV_599xCP_C"),
    ("Imperx", "IpxGEVCamera"),
    ("KowaOptronics", "SC130ET3"),
    ("NIT", "Tachyon16k"),
    ("NIT", "NSC1601GIGE"),
    ("PleoraTechnologiesInc", "CLTenFull"),
    ("PleoraTechnologiesInc", "iPORTCLGigE"),
    ("PleoraTechnologiesInc", "NTxGigE"),
    ("Sony", "XCG_CGSeries"),
    ("Sony", "XCG_CPSeries"),
    ("TeledyneDALSA", "ICE"),
    ("Xenics", "Wildcat"),
    ("FLIR", "XscSeries"),
];

/// Whether four-byte accesses must use READREG/WRITEREG with network-order
/// data. `GenICam` asks for this only for schema 1.0 XML; newer schemas read
/// registers in camera order, which differs on little-endian devices. An
/// unreadable root element keeps the legacy convention.
fn legacy_registers(xml: &str) -> bool {
    let root = (|| {
        let start = xml.find("<RegisterDescription")?;
        let tag = xml.get(start..)?.split_once('>')?.0;
        let tag = format!("{}/>", tag.trim_end_matches('/'));
        let document = roxmltree::Document::parse(&tag).ok()?;
        let root = document.root_element();
        let number = |name| root.attribute(name)?.trim().parse::<u32>().ok();
        let schema = (number("SchemaMajorVersion")?, number("SchemaMinorVersion")?);
        let attribute = |name| root.attribute(name).unwrap_or_default();
        let device = (attribute("VendorName"), attribute("ModelName"));
        Some((schema, LEGACY_REGISTER_DEVICES.contains(&device)))
    })();
    root.is_none_or(|(schema, listed)| schema < (1, 1) || listed)
}

struct LocalUrl {
    name: String,
    address: u64,
    length: usize,
}
/// Keep diagnostics from two untrusted device URLs within the worker protocol's
/// line budget without flattening the separately retained typed cause.
fn xml_diagnostic(error: &anyhow::Error) -> String {
    use std::fmt::Write as _;
    #[derive(Default)]
    struct Text {
        text: String,
        truncated: bool,
    }
    impl std::fmt::Write for Text {
        fn write_str(&mut self, text: &str) -> std::fmt::Result {
            if self.truncated {
                return Err(std::fmt::Error);
            }
            let mut end = text.len().min(4096usize.saturating_sub(self.text.len()));
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            self.text.push_str(text.get(..end).ok_or(std::fmt::Error)?);
            if end < text.len() {
                self.truncated = true;
                self.text.push('…');
                return Err(std::fmt::Error);
            }
            Ok(())
        }
    }
    let mut text = Text::default();
    let _ = write!(&mut text, "{error:#}");
    text.text
}
fn parse_local_url(url: &str) -> Result<LocalUrl> {
    let (scheme, remainder) = url
        .split_once(':')
        .context("empty or malformed GenICam URL")?;
    ensure!(
        scheme.eq_ignore_ascii_case("local"),
        "unsupported XML URL scheme {scheme:?}; camera must expose a Local: XML URL"
    );
    // An optional ?SchemaVersion=x.y.z query (GenTL URL grammar) or fragment
    // ends the path, as in an RFC 3986 split.
    let remainder = remainder.split(['?', '#']).next().unwrap_or_default();
    let mut parts = remainder.rsplitn(3, ';');
    let length = parts.next().context("missing local XML length")?;
    let address = parts.next().context("missing local XML address")?;
    let name = parts.next().context("missing local XML filename")?;
    let name = name.trim().trim_start_matches('/');
    let parse_hex = |value: &str| -> Result<u64> {
        let value = value.trim();
        let digits = value
            .strip_prefix("0x")
            .or_else(|| value.strip_prefix("0X"))
            .unwrap_or(value);
        u64::from_str_radix(digits, 16).context("invalid hexadecimal Local URL field")
    };
    let address = parse_hex(address)?;
    let length = usize::try_from(parse_hex(length)?).context("local XML size overflow")?;
    ensure!(
        length > 0 && length <= MAX_XML,
        "local XML exceeds 16 MiB safety limit"
    );
    memory_range(address, length)?;
    Ok(LocalUrl {
        name: name.to_owned(),
        address,
        length,
    })
}
fn decode_xml(name: &str, data: Vec<u8>) -> Result<String> {
    let zip_name = name
        .rsplit_once('.')
        .is_some_and(|(_, suffix)| suffix.eq_ignore_ascii_case("zip"));
    let bytes = if zip_name || data.starts_with(b"PK\x03\x04") {
        let mut archive =
            zip::ZipArchive::new(Cursor::new(data)).context("opening camera XML ZIP")?;
        ensure!(
            archive.len() <= 128,
            "camera XML ZIP contains too many entries"
        );
        let mut selected = None;
        for index in 0..archive.len() {
            let mut entry = archive.by_index(index)?;
            if entry.is_dir()
                || !entry
                    .name()
                    .rsplit_once('.')
                    .is_some_and(|(_, suffix)| suffix.eq_ignore_ascii_case("xml"))
            {
                continue;
            }
            ensure!(
                entry.size() <= MAX_XML as u64,
                "decompressed XML exceeds 16 MiB safety limit"
            );
            let mut result = Vec::new();
            Read::by_ref(&mut entry)
                .take(MAX_XML as u64 + 1)
                .read_to_end(&mut result)?;
            ensure!(
                result.len() <= MAX_XML,
                "decompressed XML exceeds 16 MiB safety limit"
            );
            ensure!(
                selected.is_none(),
                "camera XML ZIP contains multiple XML files; selection is ambiguous"
            );
            selected = Some(result);
        }
        selected.context("camera ZIP contains no XML file")?
    } else {
        data
    };
    let mut text = String::from_utf8(bytes).context("camera XML is not UTF-8")?;
    if text.starts_with('\u{feff}') {
        text.replace_range(..'\u{feff}'.len_utf8(), "");
    }
    text.truncate(text.trim_end_matches('\0').len());
    ensure!(
        text.trim_start().starts_with('<'),
        "camera XML does not contain an XML document"
    );
    Ok(text)
}

struct Stream {
    lease: Option<Arc<Lease>>,
    socket: UdpSocket,
    gvcp: SocketAddr,
    source: Option<SocketAddr>,
    /// Packets received whole: those the assembler cannot place in advance.
    buffer: Vec<u8>,
    /// A predicted packet's header, and what it holds beyond its slot.
    header: [u8; 20],
    overflow: Vec<u8>,
    /// A packet that did not land as predicted, put back together. Never
    /// `buffer`, which must keep its length to receive a whole datagram.
    scratch: Vec<u8>,
    /// Payload packets are received straight into their frames.
    scatter: bool,
    /// Several datagrams are received a system call (Linux).
    #[cfg(any(test, target_os = "linux"))]
    batch: Option<batch::Batch>,
    #[cfg(target_os = "linux")]
    predicted: Vec<Slot>,
    timeout: Duration,
    next_poll: Instant,
    next_traversal: Instant,
    request_id: u16,
    assembler: Assembler,
}
impl Stream {
    /// How long the next receive may block. A frame in flight needs the
    /// assembler polled every `POLL` for resends and expiry; with none, the
    /// socket waits out the caller's timeout instead of waking every `POLL`.
    /// Each change is a system call, so a timeout close enough is kept.
    fn wait(&self, remaining: Duration, now: Instant) -> Duration {
        // The firewall path is refreshed from the poll, which a long wait
        // would delay.
        let remaining = match self.source {
            Some(_) => remaining.min(self.next_traversal.saturating_duration_since(now).max(POLL)),
            None => remaining,
        };
        if self.assembler.in_flight() {
            let wait = remaining.min(POLL);
            if self.timeout > POLL || (wait < POLL) != (self.timeout < POLL) {
                wait
            } else {
                self.timeout
            }
        } else if self.timeout.abs_diff(remaining) <= POLL {
            self.timeout
        } else {
            remaining
        }
    }
    /// Take in every packet that already arrived from `camera`, keeping the
    /// frames they complete for `next_frame`, which returns them first.
    /// Returns how many frames it holds that `next_frame` has not returned,
    /// whole or in part: the socket's buffer can hold several.
    fn take_in(&mut self, camera: IpAddr) -> u32 {
        if self.socket.set_nonblocking(true).is_ok() {
            for _ in 0..MAX_TAKEN_IN {
                match self.receive(camera) {
                    // The frame it completes, kept in turn; a packet in
                    // error leaves its frame incomplete, as it would on its
                    // way to `next_frame`.
                    Ok(Some(Ok(Some(frame)))) => self.assembler.ready.push_front(frame),
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            let _ = self.socket.set_nonblocking(false);
            self.flush();
        }
        u32::try_from(self.assembler.ready.len() + self.assembler.frames.len()).unwrap_or(u32::MAX)
    }
    /// Receive one datagram and take it in when it came from `camera`'s
    /// stream: straight into its place in a frame when the assembler can
    /// say which payload packet comes next, else whole into the buffer, to
    /// be copied from there. None for a datagram from anywhere else.
    fn receive(&mut self, camera: IpAddr) -> std::io::Result<Option<Result<Option<Frame>>>> {
        #[cfg(target_os = "linux")]
        if self.batch.is_some() {
            return self.receive_batch(camera);
        }
        let predicted = self.scatter.then(|| self.assembler.predict()).flatten();
        let Some(slot) = predicted else {
            let (length, source) = self.socket.recv_from(&mut self.buffer)?;
            if source.ip() != camera || source == self.gvcp {
                return Ok(None);
            }
            crate::copies::record(Site::GvspReceive, length);
            let site = if self.scatter {
                Site::GvspFallback
            } else {
                Site::GvspAssemble
            };
            let packet = &self.buffer[..length];
            return Ok(Some(self.assembler.push_counted(
                packet,
                Instant::now(),
                site,
            )));
        };
        let Self {
            socket,
            header,
            overflow,
            assembler,
            scratch,
            ..
        } = self;
        let place = assembler
            .slot_mut(&slot)
            .expect("a predicted slot lies in its frame's buffer");
        let (received, flags, source) = {
            let parts = [&mut header[..slot.header], place, overflow.as_mut_slice()];
            // SAFETY: the socket only writes the bytes it receives into
            // these buffers, which are initialized already, and never
            // reads them or writes uninitialized bytes into them.
            let mut parts = parts.map(|part| unsafe {
                socket2::MaybeUninitSlice::new(std::slice::from_raw_parts_mut(
                    part.as_mut_ptr().cast::<std::mem::MaybeUninit<u8>>(),
                    part.len(),
                ))
            });
            socket2::SockRef::from(&*socket).recv_from_vectored(&mut parts)?
        };
        let source = source.as_socket();
        if source.is_none_or(|source| source.ip() != camera || source == self.gvcp) {
            // A stray datagram only overwrote a place still empty.
            return Ok(None);
        }
        crate::copies::record(Site::GvspReceive, received);
        Ok(Some(assembler.landed(
            &slot,
            (&header[..], received, flags.is_truncated()),
            overflow,
            scratch,
            Instant::now(),
        )))
    }
    /// `receive`, several datagrams at a time (`batch`).
    #[cfg(target_os = "linux")]
    fn receive_batch(&mut self, camera: IpAddr) -> std::io::Result<Option<Result<Option<Frame>>>> {
        let Self {
            socket,
            gvcp,
            assembler,
            batch,
            scatter,
            predicted: slots,
            ..
        } = self;
        let batch = batch.as_mut().expect("a stream that receives in batches");
        slots.clear();
        if *scatter {
            slots.extend(assembler.predict_run(batch::BATCH));
        }
        let slots = slots.as_slice();
        let base = if slots.is_empty() {
            std::ptr::null_mut()
        } else {
            assembler
                .newest_buffer()
                .expect("predicted slots lie in a buffer")
        };
        let count = batch::wanted(*scatter, slots);
        let mut received = [batch::Received::default(); batch::BATCH];
        let received_count = batch::receive(
            socket,
            batch,
            (slots, base, count),
            (camera, *gvcp),
            &mut received,
        )?;
        let site = if *scatter {
            Site::GvspFallback
        } else {
            Site::GvspAssemble
        };
        Ok(batch.take(
            assembler,
            (slots, base),
            &received[..received_count],
            site,
            Instant::now(),
        ))
    }
    fn flush(&mut self) {
        for (block, extended, first, last) in self.assembler.requests.drain(..) {
            self.request_id = self.request_id.wrapping_add(1).max(1);
            let packet = resend_packet(self.request_id, block, extended, first, last);
            if self
                .lease
                .as_ref()
                .is_some_and(|lease| lease.check().is_err())
            {
                break;
            }
            let _ = self.socket.send_to(&packet, self.gvcp);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ImageLeader {
    payload_type: u16,
    width: u32,
    height: u32,
    pixel_format: u32,
    timestamp: u64,
    row_bytes: usize,
    x_padding: usize,
    y_padding: usize,
    wire_bytes: usize,
    /// The image is followed by chunks (legacy type 5 or extended image mode).
    chunks: bool,
}
impl ImageLeader {
    fn parse(data: &[u8], payload_limit: usize) -> Result<Self> {
        ensure!(data.len() >= 36, "truncated GVSP image leader");
        let payload_type = be16(data, 2);
        if !matches!(payload_type, 1 | 5 | 0x4001) {
            return Err(unsupported_payload(payload_type));
        }
        let pixel_format = be32(data, 12);
        let width = be32(data, 16);
        let height = be32(data, 20);
        let bits_per_pixel = (pixel_format >> 16) & 0xff;
        ensure!(
            width > 0
                && height > 0
                && ((1..=64).contains(&bits_per_pixel)
                    || pixel_format == crate::frame::COORD3D_ABC32F),
            "invalid GVSP image dimensions or pixel format"
        );
        if let Some(format) = crate::frame::bicolor::Format::from_code(pixel_format) {
            format.validate_width(width)?;
        }
        let row_bits = u64::from(width) * u64::from(bits_per_pixel);
        let row_bytes = row_bits.div_ceil(8);
        let x_padding = be16(data, 32);
        let y_padding = be16(data, 34);
        let wire_bytes = (row_bytes + u64::from(x_padding))
            .checked_mul(u64::from(height))
            .and_then(|v| v.checked_add(u64::from(y_padding)))
            .context("GVSP image size overflow")?;
        // PFNC leaves it to the transport whether a packed row ending inside
        // a byte is padded or runs on into the next line; with no x padding
        // either layout is accepted.
        let smallest = if x_padding == 0 {
            u64::try_from(
                (u128::from(row_bits) * u128::from(height)).div_ceil(8) + u128::from(y_padding),
            )
            .context("GVSP packed image size overflow")?
        } else {
            wire_bytes
        };
        ensure!(
            smallest <= payload_limit as u64,
            "GVSP image exceeds configured payload size ({smallest} > {payload_limit})"
        );
        Ok(Self {
            payload_type,
            width,
            height,
            pixel_format,
            timestamp: be64(data, 4),
            row_bytes: usize::try_from(row_bytes).context("GVSP row exceeds host address size")?,
            x_padding: usize::from(x_padding),
            y_padding: usize::from(y_padding),
            wire_bytes: usize::try_from(wire_bytes.min(payload_limit as u64))
                .context("GVSP payload exceeds host address size")?,
            chunks: payload_type != 1,
        })
    }
    /// The fewest bytes that hold `height` lines.
    const fn image_bytes(&self, height: u32) -> usize {
        if self.x_padding == 0 {
            let bits_per_pixel = (self.pixel_format >> 16) & 0xff;
            (self.width as usize * bits_per_pixel as usize * height as usize).div_ceil(8)
        } else {
            (self.row_bytes + self.x_padding) * height as usize
        }
    }
}

fn unsupported_payload(payload_type: u16) -> anyhow::Error {
    let (name, hint) = match payload_type & 0x3fff {
        _ if payload_type & 0x8000 != 0 => ("device-specific", "select image acquisition"),
        0x0004 | 0x0005 => ("chunk data", "select image extended chunk mode"),
        0x0006 => ("JPEG", "select an uncompressed pixel format"),
        0x0007 => ("JPEG 2000", "select an uncompressed pixel format"),
        0x0008 => ("H.264", "select an uncompressed pixel format"),
        0x0009 => ("multi-zone image", "disable multi-zone streaming"),
        0x000a => ("multi-part", "disable multi-part streaming"),
        0x000b | 0x000c => ("GenDC", "disable GenDC streaming"),
        _ => ("not an image", "select image acquisition"),
    };
    Categorized::error(
        crate::types::ErrorKind::Unsupported,
        format!("unsupported GVSP payload type 0x{payload_type:04x} ({name}); {hint}"),
    )
}

#[derive(Clone, Copy)]
struct GvspPacket<'a> {
    status: u16,
    frame: u64,
    id: u32,
    kind: u8,
    part_count: u8,
    extended: bool,
    data: Data<'a>,
}
/// What a packet carries after its header.
#[derive(Clone, Copy)]
enum Data<'a> {
    /// Its bytes, which a payload packet copies into its frame, counted at
    /// the site.
    Bytes(&'a [u8], Site),
    /// A payload of this many bytes, already received in its place in the
    /// frame's buffer.
    Landed(usize),
}
impl<'a> Data<'a> {
    const fn len(self) -> usize {
        match self {
            Data::Bytes(bytes, _) => bytes.len(),
            Data::Landed(length) => length,
        }
    }
    /// The bytes of a leader or trailer, which never land in a frame.
    const fn bytes(self) -> &'a [u8] {
        match self {
            Data::Bytes(bytes, _) => bytes,
            Data::Landed(_) => &[],
        }
    }
}
/// The header's size and fields of a GVSP packet, `data` empty.
fn parse_header(packet: &[u8]) -> Result<(usize, GvspPacket<'static>)> {
    ensure!(packet.len() >= 8, "truncated GVSP header");
    let info = be32(packet, 4);
    let extended = info & 0x8000_0000 != 0;
    let (frame, id, header) = if extended {
        ensure!(packet.len() >= 20, "truncated extended GVSP header");
        (be64(packet, 8), be32(packet, 16), 20)
    } else {
        (u64::from(be16(packet, 2)), info & 0x00ff_ffff, 8)
    };
    let packet = GvspPacket {
        status: be16(packet, 0),
        frame,
        id,
        kind: ((info >> 24) & 0x7f) as u8,
        part_count: packet[7],
        extended,
        data: Data::Landed(0),
    };
    Ok((header, packet))
}
/// A GVSP packet whose payload is copied into its frame, counted at `site`.
fn parse_gvsp(packet: &[u8], site: Site) -> Result<GvspPacket<'_>> {
    let (header, parsed) = parse_header(packet)?;
    Ok(GvspPacket {
        data: Data::Bytes(&packet[header..], site),
        ..parsed
    })
}
/// Where the next packet of the newest frame is expected: the payload
/// packet after the highest one received, when its place is still empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Slot {
    block: u64,
    extended: bool,
    id: u32,
    /// The packet header's size.
    header: usize,
    /// Its payload's place in the frame's buffer.
    offset: usize,
    len: usize,
}

struct PartialFrame {
    block: u64,
    extended: bool,
    last_packet: Instant,
    leader: Option<ImageLeader>,
    trailer: Option<ImageTrailer>,
    chunks: Option<ChunkData>,
    multipart: Option<Box<multipart::MultipartFrame>>,
    packets: Option<usize>,
    /// The frame's buffer, taken from the pool when its leader or first
    /// payload packet arrives. Only the bytes of the packets received are
    /// written, and a frame is delivered only once it has them all.
    data: Option<PoolVec>,
    received: Vec<u64>,
    requested: Vec<u64>,
    count: usize,
    bytes: usize,
    end: usize,
    due: bool,
    rounds: u8,
    requested_at: Instant,
}
impl PartialFrame {
    fn ready(&self) -> bool {
        if let Some(multipart) = &self.multipart {
            return multipart.is_prepared()
                && multipart.has_trailer()
                && multipart.coverage_complete()
                && Some(self.count) == self.packets;
        }
        // A frame can end early; without its trailer only a full-height
        // frame is known to be complete.
        self.leader.as_ref().is_some_and(|leader| {
            Some(self.count) == self.packets
                && (self.trailer.is_some()
                    || (!leader.chunks
                        && self.end >= leader.image_bytes(leader.height) + leader.y_padding))
        })
    }
    /// Some cameras send a leader at acquisition start or as soon as they are
    /// armed for a trigger, long before the image data.
    fn awaits_data(&self) -> bool {
        self.has_leader() && self.count == 0 && !self.has_trailer()
    }
    fn has_leader(&self) -> bool {
        self.multipart.as_deref().map_or_else(
            || self.leader.is_some(),
            multipart::MultipartFrame::has_leader,
        )
    }
    fn has_trailer(&self) -> bool {
        self.multipart.as_deref().map_or_else(
            || self.trailer.is_some(),
            multipart::MultipartFrame::has_trailer,
        )
    }
}
/// A qualified vendor layout for this code. Its documented byte order is
/// little-endian, so a big-endian stream stays opaque and untagged.
fn encoding_for(
    encodings: &[crate::types::PixelEncoding],
    big_endian: bool,
    pixel_format: u32,
) -> Option<crate::types::PixelEncoding> {
    if big_endian {
        return None;
    }
    encodings
        .iter()
        .copied()
        .find(|encoding| encoding.check(pixel_format).is_ok())
}
/// Bytes per sample of the formats made only of 16- or 32-bit samples, which
/// a big-endian stream sends most significant byte first.
pub(super) const fn sample_bytes(pixel_format: u32) -> Option<usize> {
    match (pixel_format >> 16, pixel_format) {
        // Unpacked BiColor10/12 carry two individual16-bit components,
        // not one32-bit word. BiColor8's two byte components stay unswapped.
        (_, 0x0220_00a7 | 0x0220_00a9 | 0x0220_00ab | 0x0220_00ad) => Some(2),
        // RGBa12p and BGRa12p pack 12-bit samples.
        (_, 0x0230_0062 | 0x0230_004f) => None,
        // Coord3D_AC32f (also planar) overrides the ordinary 16-bit layout.
        (_, 0x0240_00c2 | 0x0240_00c3) | (0x0120 | 0x0260, _) => Some(4),
        // Coord3D_AC16, also planar, and ordinary 16-bit layouts.
        (_, crate::frame::COORD3D_AC16 | 0x0220_00bc) | (0x0110 | 0x0230 | 0x0240, _) => Some(2),
        _ => None,
    }
}
fn bit(bits: &[u64], id: u32) -> bool {
    bits.get(id as usize / 64)
        .is_some_and(|word| (word >> (id % 64)) & 1 != 0)
}
const fn set_bit(bits: &mut [u64], id: u32) {
    bits[id as usize / 64] |= 1 << (id % 64);
}
fn missing(received: &[u64], packets: Option<usize>) -> Vec<(u32, u32)> {
    let end = u32::try_from(packets.map_or(received.len() * 64, |packets| packets + 2))
        .expect("received packet maps are bounded by MAX_FRAME_PACKETS");
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for id in (0..end).filter(|id| !bit(received, *id)) {
        match runs.last_mut() {
            Some((_, last)) if *last + 1 == id => *last = id,
            _ => runs.push((id, id)),
        }
    }
    if packets.is_none() && runs.last().is_some_and(|(_, last)| *last + 1 == end) {
        runs.pop();
    }
    runs
}
/// The leader gives the most lines a frame can have; line-scan cameras end a
/// frame early (frame active triggers, line timeout) and report the lines
/// they sent in the trailer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ImageTrailer {
    payload_type: u16,
    id: u32,
    height: u32,
    /// Reported chunk payload length and its XML chunk layout identifier.
    chunks: Option<(usize, u32)>,
}
impl ImageTrailer {
    fn parse(id: u32, data: &[u8]) -> Result<Self> {
        ensure!(id > 1, "invalid GVSP image trailer");
        Ok(Self {
            id,
            ..Self::parse_body(data)?
        })
    }
    /// The trailer body also appears inside an All-in datagram, with no
    /// separate trailer packet ID. Only ordinary transmission uses `id`.
    fn parse_body(data: &[u8]) -> Result<Self> {
        ensure!(data.len() >= 8, "invalid GVSP image trailer");
        let payload_type = be16(data, 2);
        ensure!(
            matches!(payload_type, 1 | 5 | 0x4001),
            "GVSP trailer is not a supported image"
        );
        let legacy_chunks = payload_type == 5;
        let chunks = if payload_type == 1 {
            None
        } else {
            ensure!(data.len() >= 16, "truncated GVSP extended chunk trailer");
            Some((
                usize::try_from(be32(data, if legacy_chunks { 4 } else { 8 }))
                    .context("GVSP chunk length exceeds host address size")?,
                be32(data, 12),
            ))
        };
        Ok(Self {
            payload_type,
            id: 0,
            height: be32(data, if legacy_chunks { 8 } else { 4 }),
            chunks,
        })
    }
}
fn check_trailer_image(leader: &ImageLeader, trailer: ImageTrailer) -> Result<()> {
    let ImageTrailer {
        height,
        chunks,
        payload_type,
        ..
    } = trailer;
    ensure!(
        leader.payload_type == payload_type,
        "GVSP leader and trailer payload types disagree"
    );
    ensure!(
        leader.chunks == chunks.is_some(),
        "GVSP leader and trailer disagree about chunk data"
    );
    ensure!(
        (1..=leader.height).contains(&height),
        "GVSP trailer height {height} does not fit the leader's {}",
        leader.height
    );
    Ok(())
}
fn check_trailer(leader: &ImageLeader, block_size: usize, trailer: ImageTrailer) -> Result<()> {
    check_trailer_image(leader, trailer)?;
    let height = trailer.height;
    let packets = trailer.id as usize - 1;
    let most = (leader.row_bytes + leader.x_padding) * height as usize + leader.y_padding;
    ensure!(
        packets * block_size >= leader.image_bytes(height)
            && (leader.chunks || packets <= most.min(leader.wire_bytes).div_ceil(block_size)),
        "GVSP trailer packet ID does not match image size"
    );
    Ok(())
}
struct Budget {
    tokens: f64,
    at: Instant,
}
impl Budget {
    fn take(&mut self, now: Instant, packets: u32) -> bool {
        let refill = now.saturating_duration_since(self.at).as_secs_f64() * RESEND_BUDGET;
        self.tokens = (self.tokens + refill).min(RESEND_BUDGET);
        self.at = self.at.max(now);
        let granted = self.tokens >= f64::from(packets);
        if granted {
            self.tokens -= f64::from(packets);
        }
        granted
    }
}
const fn block_distance(from: u64, to: u64, extended: bool) -> u64 {
    let distance = to.wrapping_sub(from);
    if extended {
        distance
    } else {
        distance.wrapping_add(65_535) % 65_535
    }
}
#[derive(Default)]
struct Blocks {
    next: Option<u64>,
    seen: u128,
}
impl Blocks {
    fn seen(&mut self, block: u64, extended: bool) {
        let offset = block_distance(*self.next.get_or_insert(block), block, extended);
        if offset < 128 {
            self.seen |= 1 << offset;
        }
    }
    fn delivered(&mut self, block: u64, extended: bool) -> u64 {
        let next = self.next.unwrap_or(block);
        let offset = block_distance(next, block, extended);
        if offset >= 32_768 {
            *self = Self {
                next: Some(block),
                seen: 1,
            };
            return 0;
        }
        let finished = offset.saturating_sub(16);
        let (window, retained) = if finished >= 128 {
            (u128::MAX, 0)
        } else {
            ((1 << finished) - 1, self.seen >> finished)
        };
        let lost = finished - u64::from((self.seen & window).count_ones());
        self.seen = retained | (1 << (offset - finished));
        self.next = Some(if extended {
            next.wrapping_add(finished)
        } else {
            next.wrapping_add(65_534 + finished) % 65_535 + 1
        });
        lost
    }
}
struct Assembler {
    frames: Vec<PartialFrame>,
    closed: VecDeque<u64>,
    ready: VecDeque<Frame>,
    requests: Vec<(u64, bool, u32, u32)>,
    payload_limit: usize,
    limits: [Option<(usize, usize)>; 2],
    timestamp_frequency: u64,
    resend: bool,
    /// The camera kept `GevSCPSBigEndian` set: 16-bit samples arrive swapped.
    big_endian: bool,
    big_endian_mono16: bool,
    multipart_projection: bool,
    pixel_encodings: Vec<crate::types::PixelEncoding>,
    budget: Budget,
    blocks: Blocks,
    stats: TransportStats,
    /// Payload packets, and their bytes, copied into their frames rather
    /// than received in place.
    copied: (u64, u64),
}
impl Assembler {
    fn new(payload_limit: usize, packet_size: usize, timestamp_frequency: u64) -> Self {
        Self {
            frames: Vec::new(),
            closed: VecDeque::new(),
            ready: VecDeque::new(),
            requests: Vec::new(),
            payload_limit,
            limits: [36, 48].map(|overhead| {
                packet_size
                    .checked_sub(overhead)
                    .filter(|block| *block > 0)
                    .map(|block| (block, payload_limit.div_ceil(block).min(MAX_FRAME_PACKETS)))
            }),
            timestamp_frequency,
            resend: false,
            big_endian: false,
            big_endian_mono16: false,
            multipart_projection: false,
            pixel_encodings: Vec::new(),
            budget: Budget {
                tokens: RESEND_BUDGET,
                at: Instant::now(),
            },
            blocks: Blocks::default(),
            stats: TransportStats::default(),
            copied: (0, 0),
        }
    }
    /// Where the next packet is expected to land: the newest frame's
    /// payload packet after the highest one received, once the frame has
    /// its buffer, if that packet's place is still empty and inside the
    /// frame.
    fn predict(&self) -> Option<Slot> {
        self.predict_run(1).next()
    }
    /// Where the next `count` packets are expected to land, as `predict`
    /// says of the first: the newest frame's payload packets that follow,
    /// up to its end or the first one already received.
    fn predict_run(&self, count: usize) -> impl Iterator<Item = Slot> + '_ {
        let predicted = self
            .frames
            .last()
            .filter(|frame| frame.data.is_some())
            .and_then(|frame| {
                self.limits[usize::from(frame.extended)].map(|limits| (frame, limits))
            });
        predicted
            .into_iter()
            .flat_map(move |(frame, (block_size, max_packets))| {
                let first = frame.end.div_ceil(block_size) + 1;
                (first..=max_packets).take(count).map_while(move |id| {
                    let offset = (id - 1) * block_size;
                    let wire_id =
                        u32::try_from(id).expect("predicted IDs are bounded by MAX_FRAME_PACKETS");
                    let outside = frame.packets.is_some_and(|packets| id > packets)
                        || offset >= self.payload_limit
                        || bit(&frame.received, wire_id);
                    (!outside).then(|| Slot {
                        block: frame.block,
                        extended: frame.extended,
                        id: wire_id,
                        header: if frame.extended { 20 } else { 8 },
                        offset,
                        len: block_size.min(self.payload_limit - offset),
                    })
                })
            })
    }
    /// The newest frame's buffer, where a batch's predicted slots lie.
    #[cfg(any(test, target_os = "linux"))]
    fn newest_buffer(&mut self) -> Option<*mut u8> {
        Some(self.frames.last_mut()?.data.as_mut()?.as_mut_ptr())
    }
    /// Whether `slot`'s frame still has the buffer starting at `base` and
    /// its place is still empty, so a packet that landed there can be taken
    /// in.
    #[cfg(any(test, target_os = "linux"))]
    fn still_empty(&self, slot: &Slot, base: *const u8) -> bool {
        self.frames.iter().rev().any(|frame| {
            frame.block == slot.block
                && frame
                    .data
                    .as_ref()
                    .is_some_and(|data| std::ptr::eq(data.as_ptr(), base))
                && !bit(&frame.received, slot.id)
        })
    }
    /// The place in the newest frame's buffer `slot` predicts.
    fn slot_mut(&mut self, slot: &Slot) -> Option<&mut [u8]> {
        let frame = self
            .frames
            .last_mut()
            .filter(|frame| frame.block == slot.block)?;
        let data = frame.data.as_mut()?;
        data.get_mut(slot.offset..slot.offset + slot.len)
    }
    /// The payload packet a datagram of `received` bytes is, when it landed
    /// in `slot` as predicted: its header, in `header`, names the slot's
    /// frame and packet.
    fn lands(
        slot: &Slot,
        (header, received, truncated): (&[u8], usize, bool),
    ) -> Option<GvspPacket<'static>> {
        let (size, packet) = (!truncated && received > slot.header)
            .then(|| parse_header(&header[..slot.header]).ok())
            .flatten()?;
        let landed = size == slot.header
            && matches!(packet.status, 0 | 0x0100)
            && packet.kind == 3
            && (packet.frame, packet.extended, packet.id) == (slot.block, slot.extended, slot.id);
        landed.then(|| GvspPacket {
            data: Data::Landed(received - slot.header),
            ..packet
        })
    }
    /// Take in a datagram of `received` bytes that was received as `slot`
    /// predicted: its first `slot.header` bytes in `header`, then `slot.len`
    /// in the slot, the rest in `overflow`. A payload packet of the slot's
    /// own frame and ID has landed and is only marked; anything else is
    /// put together in `scratch` and taken in as a whole packet, copied.
    fn landed(
        &mut self,
        slot: &Slot,
        (header, received, truncated): (&[u8], usize, bool),
        overflow: &[u8],
        scratch: &mut Vec<u8>,
        now: Instant,
    ) -> Result<Option<Frame>> {
        if truncated {
            return Ok(None);
        }
        if let Some(packet) = Self::lands(slot, (header, received, false)) {
            return self.take(packet, now);
        }
        // At most one packet: a resend, a reorder, a leader or a trailer.
        scratch.clear();
        scratch.extend_from_slice(&header[..received.min(slot.header)]);
        let rest = received.saturating_sub(slot.header);
        let in_slot = rest.min(slot.len);
        let place = self.slot_mut(slot).context("GVSP frame buffer missing")?;
        scratch.extend_from_slice(&place[..in_slot]);
        scratch.extend_from_slice(&overflow[..(rest - in_slot).min(overflow.len())]);
        crate::copies::record(Site::GvspFallback, scratch.len());
        self.push_counted(scratch, now, Site::GvspFallback)
    }
    fn incomplete_summary(&self) -> String {
        match self.frames.last() {
            Some(frame) if self.in_flight() => format!(
                "{} incomplete frame(s), newest has {} bytes and {} packets, leader {}, trailer {}; {} expired/evicted frame(s)",
                self.frames.len(),
                frame.bytes,
                frame.count,
                frame.has_leader(),
                frame.has_trailer(),
                self.stats.incomplete_frames
            ),
            Some(_) => format!(
                "an image leader arrived but no image data; {} expired/evicted frame(s); check trigger mode and trigger source",
                self.stats.incomplete_frames
            ),
            None => format!(
                "no complete image received; {} expired/evicted frame(s); check AcquisitionStart, trigger mode and stream firewall",
                self.stats.incomplete_frames
            ),
        }
    }
    #[cfg(test)]
    fn push(&mut self, packet: &[u8]) -> Result<Option<Frame>> {
        self.push_at(packet, Instant::now())
    }
    /// Take in a packet received whole, copying its payload into its frame.
    #[cfg(test)]
    fn push_at(&mut self, packet: &[u8], now: Instant) -> Result<Option<Frame>> {
        self.push_counted(packet, now, Site::GvspAssemble)
    }
    fn push_counted(&mut self, packet: &[u8], now: Instant, site: Site) -> Result<Option<Frame>> {
        let packet = parse_gvsp(packet, site)?;
        self.take(packet, now)
    }
    fn take(&mut self, packet: GvspPacket<'_>, now: Instant) -> Result<Option<Frame>> {
        self.stats.packets_received += 1;
        let block = packet.frame;
        let index = self.frames.iter().rposition(|frame| frame.block == block);
        if index.is_none() && self.closed.contains(&block) {
            return Ok(self.ready.pop_front());
        }
        if packet.status >= 0x8000 {
            // A resend the camera cannot serve yet (packet not yet or
            // temporarily unavailable) is retried on the normal schedule.
            if !matches!(packet.status, 0x8010 | 0x8014) {
                self.discard(block);
            }
        } else if let Err(error) = self.insert(index, packet, now) {
            self.discard(block);
            return Err(error);
        }
        Ok(self.ready.pop_front())
    }

    fn insert(&mut self, index: Option<usize>, packet: GvspPacket<'_>, now: Instant) -> Result<()> {
        ensure!(
            packet.status == 0 || packet.status == 0x0100,
            "GVSP camera error status 0x{:04x}",
            packet.status
        );
        if packet.kind == 4 {
            return self.insert_all_in(index, packet, now);
        }
        if packet.kind == multipart::CONTENT || multipart::is_metadata(packet) {
            return self.insert_multipart(index, packet, now);
        }
        ensure!(
            index.is_none_or(|index| self.frames[index].multipart.is_none()),
            "GVSP block mixes multipart and ordinary packets"
        );
        ensure!(
            (1..=3).contains(&packet.kind),
            "unsupported GVSP packet content {}; select ordinary image or supported multipart transmission",
            packet.kind
        );
        let (block_size, max_packets) =
            self.limits[usize::from(packet.extended)].context("invalid GVSP packet size")?;
        ensure!(
            packet.id as usize <= max_packets + 1,
            "GVSP packet ID exceeds payload bounds"
        );
        let index =
            index.unwrap_or_else(|| self.create(packet.frame, packet.extended, max_packets, now));
        let frame = &mut self.frames[index];
        ensure!(
            frame.extended == packet.extended,
            "GVSP frame mixes standard and extended IDs"
        );
        frame.last_packet = now;
        match packet.kind {
            1 => {
                ensure!(packet.id == 0, "GVSP leader packet ID must be zero");
                let leader = ImageLeader::parse(packet.data.bytes(), self.payload_limit)?;
                ensure!(
                    !self.big_endian
                        || crate::frame::packed_coordinate_group(leader.pixel_format).is_none(),
                    Categorized::error(
                        crate::types::ErrorKind::Unsupported,
                        "GVSP packed coordinates require little-endian packing"
                    )
                );
                if let Some(previous) = &frame.leader {
                    ensure!(previous == &leader, "conflicting duplicate GVSP leader");
                } else {
                    let packets = leader.wire_bytes.div_ceil(block_size);
                    ensure!(
                        packets <= MAX_FRAME_PACKETS,
                        "GVSP image requires too many packets"
                    );
                    // Only the highest packet can be short, so the highest
                    // received ID describes every packet so far. Chunk data
                    // makes the payload longer than the image.
                    ensure!(
                        leader.chunks || frame.end.div_ceil(block_size) <= packets,
                        "GVSP payload packets do not match the image leader"
                    );
                    if let Some(trailer) = frame.trailer {
                        check_trailer(&leader, block_size, trailer)?;
                    }
                    if !leader.chunks {
                        frame.packets.get_or_insert(packets);
                    }
                    frame.leader = Some(leader);
                    frame
                        .data
                        .get_or_insert_with(|| pool().take_stale(self.payload_limit));
                    set_bit(&mut frame.received, 0);
                }
            }
            2 => {
                let data = packet.data.bytes();
                let trailer = ImageTrailer::parse(packet.id, data)?;
                if let Some(previous) = frame.trailer {
                    ensure!(previous == trailer, "conflicting duplicate GVSP trailer");
                } else {
                    let packets = packet.id as usize - 1;
                    ensure!(
                        frame.packets.is_none_or(|known| packets <= known)
                            && frame.end.div_ceil(block_size) <= packets,
                        "GVSP trailer packet ID does not match image size"
                    );
                    if let Some(leader) = &frame.leader {
                        check_trailer(leader, block_size, trailer)?;
                    }
                    frame.packets = Some(packets);
                    frame.trailer = Some(trailer);
                    set_bit(&mut frame.received, packet.id);
                }
            }
            _ => {
                let length = packet.data.len();
                ensure!(
                    packet.id > 0 && length > 0 && length <= block_size,
                    "invalid GVSP image payload packet"
                );
                ensure!(
                    frame
                        .packets
                        .is_none_or(|packets| packet.id as usize <= packets),
                    "GVSP payload packet lies beyond image"
                );
                let offset = (packet.id as usize - 1) * block_size;
                ensure!(
                    offset < self.payload_limit,
                    "GVSP payload exceeds configured bounds"
                );
                // Padding in a last packet may run past the payload size;
                // only bytes inside it are kept.
                let range = offset..(offset + length).min(self.payload_limit);
                if bit(&frame.received, packet.id) {
                    // A resent last packet may add or drop padding; the bytes
                    // both copies hold must match. Only empty places are
                    // predicted, so a duplicate never lands.
                    let Data::Bytes(bytes, _) = packet.data else {
                        bail!("GVSP payload landed on a packet already received");
                    };
                    let kept = &bytes[..range.len()];
                    let held = (frame.end - offset).min(block_size);
                    let common = held.min(kept.len());
                    let data = frame.data.as_ref().context("GVSP frame buffer missing")?;
                    ensure!(
                        (length == held || frame.end <= offset + block_size)
                            && data[offset..offset + common] == kept[..common],
                        "conflicting duplicate GVSP payload packet"
                    );
                } else {
                    ensure!(
                        length == block_size
                            || (frame.end <= offset
                                && frame
                                    .trailer
                                    .is_none_or(|trailer| trailer.id == packet.id + 1)),
                        "GVSP payload packet is short but not the last"
                    );
                    if length < block_size {
                        frame.packets = Some(packet.id as usize);
                    }
                    let data = frame
                        .data
                        .get_or_insert_with(|| pool().take_stale(self.payload_limit));
                    if let Data::Bytes(bytes, site) = packet.data {
                        crate::copies::record(site, range.len());
                        self.copied.0 += 1;
                        self.copied.1 += range.len() as u64;
                        data[range.clone()].copy_from_slice(&bytes[..range.len()]);
                    }
                    set_bit(&mut frame.received, packet.id);
                    frame.count += 1;
                    frame.bytes += range.len();
                    frame.end = frame.end.max(offset + length);
                    if bit(&frame.requested, packet.id) {
                        self.stats.resend_recovered += 1;
                    }
                }
            }
        }
        if let (Some(leader), Some(trailer), Some(packets)) =
            (&frame.leader, frame.trailer, frame.packets)
            && bit(
                &frame.received,
                u32::try_from(packets).context("frame packet count exceeds wire ID range")?,
            )
        {
            let height = trailer.height;
            ensure!(
                frame.end >= leader.image_bytes(height),
                "GVSP payload ends before the trailer's {height} lines"
            );
        }
        if frame.ready()
            && frame.chunks.is_none()
            && let (Some(leader), Some(trailer)) = (&frame.leader, frame.trailer)
            && let Some((reported_length, layout_id)) = trailer.chunks
        {
            let end = frame.end.min(self.payload_limit);
            let min_image = leader.image_bytes(trailer.height) + leader.y_padding;
            let max_image =
                (leader.row_bytes + leader.x_padding) * trailer.height as usize + leader.y_padding;
            // An image-extended-chunk block can report the whole payload.
            // Require exact image geometry and corroborate it with the
            // first image chunk's tag. Also accept suffix-length trailers.
            let total_length = reported_length == end && min_image == max_image;
            if leader.payload_type == 5 {
                // Legacy extended-chunk trailers report the complete payload,
                // not a suffix length. The image's first footer must also
                // corroborate its exact geometry before bytes become pixels.
                ensure!(
                    total_length,
                    "GVSP legacy chunk payload length or image boundary is inconsistent"
                );
            }
            let image_bytes = if total_length {
                min_image
            } else {
                end.checked_sub(reported_length)
                    .context("GVSP chunk length exceeds payload")?
            };
            ensure!(
                (min_image..=max_image).contains(&image_bytes),
                "GVSP chunk boundary does not match image dimensions"
            );
            let data = frame.data.as_ref().context("GVSP frame buffer missing")?;
            let mut chunks = ChunkData::parse(
                &data[..end],
                image_bytes,
                image_bytes,
                ByteOrder::Big,
                layout_id,
            )?;
            ensure!(
                !total_length
                    || chunks
                        .entries
                        .first()
                        .is_some_and(|image| { image.offset == 0 && image.length == image_bytes }),
                "GVSP total-length chunk payload lacks its first image tag"
            );
            let pixel_encoding =
                encoding_for(&self.pixel_encodings, self.big_endian, leader.pixel_format);
            chunks.image_layout = Some(ImageLayout {
                pixel_encoding,
                offset: 0,
                pfnc_code: pixel_encoding.is_none().then_some(leader.pixel_format),
                width: leader.width,
                height: trailer.height,
                row_bytes: leader.row_bytes,
                row_stride: leader.row_bytes + leader.x_padding,
                padding_y: leader.y_padding,
                byte_order: Some(
                    if self.big_endian || (self.big_endian_mono16 && leader.pixel_format == MONO16)
                    {
                        ByteOrder::Big
                    } else {
                        ByteOrder::Little
                    },
                ),
                continuous_packing: image_bytes < max_image,
            });
            frame.chunks = Some(chunks);
        }
        let complete = frame.due && frame.ready();
        if packet.kind == 2 {
            self.trigger(index, now);
        } else if complete {
            self.complete(index);
        }
        Ok(())
    }
    /// Complete an unchunked All-in IMAGE datagram.
    ///
    /// The common header is followed by the image leader, trailer and
    /// payload, in that order (upstream Wireshark's `dissect_packet_all_in`).
    /// The packet ID never places bytes, sizes packet maps or requests packets.
    fn insert_all_in(
        &mut self,
        index: Option<usize>,
        packet: GvspPacket<'_>,
        now: Instant,
    ) -> Result<()> {
        ensure!(
            index.is_none(),
            "GVSP block mixes All-in and separate packets"
        );
        let (datagram_body, _) =
            self.limits[usize::from(packet.extended)].context("invalid GVSP packet size")?;
        let bytes = packet.data.bytes();
        ensure!(
            bytes.len() <= datagram_body,
            "GVSP All-in exceeds negotiated packet size"
        );
        let (metadata, pixels) = bytes
            .split_first_chunk::<44>()
            .context("truncated GVSP All-in image metadata")?;
        ensure!(
            be16(metadata, 2) == 1,
            Categorized::error(
                crate::types::ErrorKind::Unsupported,
                "GVSP All-in supports only unchunked IMAGE payloads"
            )
        );
        ensure!(
            metadata[0] == 0,
            Categorized::error(
                crate::types::ErrorKind::Unsupported,
                "GVSP All-in requires a single image field"
            )
        );
        let leader = ImageLeader::parse(&metadata[..36], self.payload_limit)?;
        ensure!(
            be16(metadata, 38) == 1,
            "GVSP leader and trailer payload types disagree"
        );
        let trailer = ImageTrailer::parse_body(&metadata[36..44])?;
        check_trailer_image(&leader, trailer)?;
        ensure!(
            !self.big_endian
                || crate::frame::packed_coordinate_group(leader.pixel_format).is_none(),
            Categorized::error(
                crate::types::ErrorKind::Unsupported,
                "GVSP packed coordinates require little-endian packing"
            )
        );
        let least = leader
            .image_bytes(trailer.height)
            .checked_add(leader.y_padding)
            .context("GVSP All-in image size overflow")?;
        let most = (leader.row_bytes + leader.x_padding)
            .checked_mul(trailer.height as usize)
            .and_then(|size| size.checked_add(leader.y_padding))
            .context("GVSP All-in image size overflow")?;
        ensure!(
            pixels.len() <= self.payload_limit && (pixels.len() == least || pixels.len() == most),
            "GVSP All-in payload length does not match image geometry"
        );
        // All bounds and trailer consistency are proved before allocation or
        // insertion. No packet-ID inference, reserved bytes or unreceived
        // tails are needed for this self-contained datagram.
        let Data::Bytes(_, site) = packet.data else {
            anyhow::bail!("GVSP All-in datagram was not reconstructed from a predicted slot");
        };
        let mut data = pool().take_stale(pixels.len());
        crate::copies::record(site, pixels.len());
        data.copy_from_slice(pixels);
        self.copied.0 += 1;
        self.copied.1 += pixels.len() as u64;
        let index = self.create(packet.frame, packet.extended, 0, now);
        let frame = &mut self.frames[index];
        frame.leader = Some(leader);
        frame.trailer = Some(trailer);
        frame.data = Some(data);
        frame.end = pixels.len();
        // There are zero separately transmitted DATA packets in this mode.
        // Finalize now through the ordinary row/packing/word normalization.
        frame.packets = Some(0);
        self.complete(index);
        Ok(())
    }

    fn insert_multipart(
        &mut self,
        index: Option<usize>,
        packet: GvspPacket<'_>,
        now: Instant,
    ) -> Result<()> {
        ensure!(
            packet.extended,
            Categorized::error(
                crate::types::ErrorKind::Unsupported,
                "GVSP multipart requires extended IDs"
            )
        );
        let (image_block, _) = self.limits[1].context("invalid GVSP packet size")?;
        let block_size = image_block
            .checked_sub(multipart::FRAGMENT_HEADER)
            .filter(|size| *size > 0)
            .context("invalid GVSP multipart packet size")?;
        // Every datagram contributes at least one byte. Small last packets
        // are valid in every part, so the ordinary stride bound cannot be
        // used here.
        let max_packets = self.payload_limit.min(MAX_FRAME_PACKETS);
        ensure!(
            packet.id as usize <= max_packets + 1,
            "GVSP multipart packet ID exceeds payload bounds"
        );
        let index = index.unwrap_or_else(|| {
            let index = self.create(packet.frame, true, max_packets, now);
            self.frames[index].multipart = Some(Box::default());
            index
        });
        let frame = &mut self.frames[index];
        let state = frame
            .multipart
            .as_mut()
            .context("GVSP block mixes ordinary and multipart packets")?;
        frame.last_packet = now;
        match packet.kind {
            1 => {
                let order = if self.big_endian {
                    Some(ByteOrder::Big)
                } else if self.big_endian_mono16 {
                    None
                } else {
                    Some(ByteOrder::Little)
                };
                state.receive_leader(
                    packet,
                    self.payload_limit,
                    order,
                    self.multipart_projection,
                )?;
                set_bit(&mut frame.received, 0);
            }
            2 => {
                let packets = (packet.id as usize).saturating_sub(1);
                ensure!(
                    frame.packets.is_none_or(|known| known == packets),
                    "GVSP multipart trailer ID disagrees with payload count"
                );
                state.receive_trailer(packet.id, packet.data.bytes())?;
                frame.packets = Some(packets);
                set_bit(&mut frame.received, packet.id);
            }
            _ => {
                ensure!(
                    packet.id as usize <= max_packets
                        && frame
                            .packets
                            .is_none_or(|packets| packet.id as usize <= packets),
                    "GVSP multipart fragment ID lies beyond payload"
                );
                let accepted = state.receive_fragment(
                    packet,
                    block_size,
                    self.payload_limit,
                    bit(&frame.received, packet.id),
                )?;
                if accepted > 0 {
                    set_bit(&mut frame.received, packet.id);
                    frame.count += 1;
                    frame.bytes += accepted;
                    self.copied.0 += 1;
                    self.copied.1 += accepted as u64;
                    if bit(&frame.requested, packet.id) {
                        self.stats.resend_recovered += 1;
                    }
                }
            }
        }
        if state.coverage_complete() && frame.count == state.highest as usize {
            // With all part bytes and no ID gaps, the next ID is the
            // trailer. This lets the usual resend policy request it.
            frame.packets.get_or_insert(frame.count);
        }
        if state.has_leader() && state.has_trailer() && Some(frame.count) == frame.packets {
            ensure!(
                state.coverage_complete(),
                "GVSP multipart contains a byte coverage hole"
            );
            state.prepare(frame.block, self.timestamp_frequency)?;
        }
        let complete = frame.due && frame.ready();
        if packet.kind == 2 {
            self.trigger(index, now);
        } else if complete {
            self.complete(index);
        }
        Ok(())
    }
    fn create(&mut self, block: u64, extended: bool, max_packets: usize, now: Instant) -> usize {
        while let Some(index) = self.frames.iter().position(|frame| !frame.due) {
            self.trigger(index, now);
        }
        while self.frames.len() >= MAX_INCOMPLETE_FRAMES {
            self.abandon(0);
        }
        self.blocks.seen(block, extended);
        self.frames.push(PartialFrame {
            block,
            extended,
            last_packet: now,
            leader: None,
            trailer: None,
            chunks: None,
            multipart: None,
            packets: None,
            data: None,
            received: vec![0; (max_packets + 2).div_ceil(64)],
            requested: Vec::new(),
            count: 0,
            bytes: 0,
            end: 0,
            due: false,
            rounds: 0,
            requested_at: now,
        });
        self.frames.len() - 1
    }
    /// Whether a frame needs `poll` for resends and expiry.
    fn in_flight(&self) -> bool {
        match self.frames.as_slice() {
            [] => false,
            [frame] => !frame.awaits_data(),
            _ => true,
        }
    }
    fn poll(&mut self, now: Instant) {
        let idle = |frame: &PartialFrame| now.saturating_duration_since(frame.last_packet);
        // The newest frame does not age while only its leader has arrived;
        // a newer block retires it.
        while let Some(index) = (0..self.frames.len()).find(|&index| {
            let frame = &self.frames[index];
            !(index + 1 == self.frames.len() && frame.awaits_data())
                && (idle(frame) >= FRAME_IDLE
                    || (!frame.due && idle(frame) >= RESEND_IDLE)
                    || (frame.rounds > 0
                        && now.saturating_duration_since(frame.requested_at) >= RESEND_RETRY))
        }) {
            let frame = &self.frames[index];
            if idle(frame) >= FRAME_IDLE {
                self.abandon(index);
            } else if frame.due {
                self.request(index, now);
            } else {
                self.trigger(index, now);
            }
        }
    }
    fn trigger(&mut self, index: usize, now: Instant) {
        let frame = &mut self.frames[index];
        frame.due = true;
        if frame.ready() {
            self.complete(index);
        } else if self.resend && frame.rounds == 0 {
            self.request(index, now);
        }
    }
    fn request(&mut self, index: usize, now: Instant) {
        let frame = &mut self.frames[index];
        let mut runs = missing(&frame.received, frame.packets);
        let limit = (frame.packets.unwrap_or(frame.count) / 8).max(16);
        let excessive = |runs: &[(u32, u32)]| {
            runs.len() > RESEND_RUNS
                || runs
                    .iter()
                    .map(|(first, last)| (last - first + 1) as usize)
                    .sum::<usize>()
                    > limit
        };
        // Until the trailer arrives, the packets above the highest one received
        // may not have been sent yet: line-scan cameras pause between lines and
        // can end a frame early. That run is requested only while it is short,
        // and then the frame waits for its data or `FRAME_IDLE`.
        if !frame.has_trailer()
            && frame.packets.is_some()
            && (frame.rounds >= RESEND_ROUNDS || excessive(&runs))
        {
            runs.pop();
        }
        if (frame.rounds == 0 && excessive(&runs))
            || (frame.rounds >= RESEND_ROUNDS && !runs.is_empty())
        {
            self.abandon(index);
            return;
        }
        if runs.is_empty() {
            frame.rounds = 0;
            return;
        }
        if frame.requested.is_empty() {
            frame.requested = vec![0; frame.received.len()];
        }
        for (first, last) in runs {
            if self.budget.take(now, last - first + 1) {
                (first..=last).for_each(|id| set_bit(&mut frame.requested, id));
                self.requests
                    .push((frame.block, frame.extended, first, last));
                self.stats.resend_requested += u64::from(last - first + 1);
            }
        }
        frame.rounds += 1;
        frame.requested_at = now;
    }
    fn close(&mut self, block: u64) {
        if self.closed.len() == CLOSED_BLOCKS {
            self.closed.pop_front();
        }
        self.closed.push_back(block);
    }
    fn abandon(&mut self, index: usize) {
        let frame = self.frames.remove(index);
        self.close(frame.block);
        self.stats.incomplete_frames += 1;
    }
    fn discard(&mut self, block: u64) {
        match self.frames.iter().rposition(|frame| frame.block == block) {
            Some(index) => self.abandon(index),
            None => self.close(block),
        }
    }
    fn complete(&mut self, index: usize) {
        for _ in 0..index {
            self.abandon(0);
        }
        let frame = self.frames.remove(0);
        self.close(frame.block);
        self.stats.lost_frames += self.blocks.delivered(frame.block, frame.extended);
        if let Some(multipart) = frame.multipart {
            self.ready.push_back(multipart.finish());
            return;
        }
        let leader = frame.leader.expect("complete frames have a leader");
        let height = frame
            .trailer
            .map_or(leader.height, |trailer| trailer.height);
        let mut data = frame.data.unwrap_or_else(|| pool().take_stale(0));
        let mut chunks = frame.chunks;
        let image_end = chunks
            .as_ref()
            .map_or(frame.end, |chunks| chunks.image_bytes);
        if let Some(chunks) = &mut chunks {
            data = chunks.retain(data);
        }
        let stride = leader.row_bytes + leader.x_padding;
        if leader.x_padding > 0 {
            crate::copies::record(
                Site::GigeRowCompact,
                leader.row_bytes * (height as usize).saturating_sub(1),
            );
            for row in 1..height as usize {
                data.copy_within(
                    row * stride..row * stride + leader.row_bytes,
                    row * leader.row_bytes,
                );
            }
        }
        // A short image-packed PFNC stream can end before the size of its
        // byte-aligned rows. Normalize that unambiguous layout for Frame's
        // row-based consumers. A full-sized payload remains line-padded:
        // its length cannot establish whether extra bytes are transport
        // padding. A chunk trailer gives the image's boundary explicitly.
        let line_bytes = leader.row_bytes * height as usize;
        if leader.x_padding == 0
            && line_bytes <= MAX_PAYLOAD
            && image_end == leader.image_bytes(height) + leader.y_padding
            && leader.image_bytes(height) < line_bytes
            && crate::frame::lsb_packed(leader.pixel_format)
        {
            let row_bits = leader.width as usize * ((leader.pixel_format >> 16) & 0xff) as usize;
            data.resize(line_bytes, 0);
            // Move backwards so expanding rows can reuse the receive buffer.
            for row in (0..height as usize).rev() {
                let first_bit = row * row_bits;
                let shift = first_bit % 8;
                for byte in (0..leader.row_bytes).rev() {
                    let source = first_bit / 8 + byte;
                    let low = u16::from(data[source]);
                    let high = if shift == 0 {
                        0
                    } else {
                        u16::from(data.get(source + 1).copied().unwrap_or(0)) << 8
                    };
                    data[row * leader.row_bytes + byte] = ((low | high) >> shift).to_le_bytes()[0];
                }
                if !row_bits.is_multiple_of(8) {
                    data[(row + 1) * leader.row_bytes - 1] &= (1 << (row_bits % 8)) - 1;
                }
            }
            data.truncate(line_bytes);
        } else {
            data.truncate(line_bytes.min(frame.end));
        }
        let swapped = if self.big_endian_mono16 && leader.pixel_format == MONO16 {
            Some(2)
        } else if self.big_endian {
            sample_bytes(leader.pixel_format)
        } else {
            None
        };
        if let Some(bytes) = swapped {
            data.chunks_exact_mut(bytes).for_each(<[u8]>::reverse);
        }
        // Without its tick frequency the camera's clock cannot be read as
        // nanoseconds; the host time of the frame still says when it came.
        let device_ts_ns = (self.timestamp_frequency != 0)
            .then(|| ticks_ns(leader.timestamp, self.timestamp_frequency));
        let pixel_encoding =
            encoding_for(&self.pixel_encodings, self.big_endian, leader.pixel_format);
        self.ready.push_back(Frame {
            pixel_encoding,
            device_timestamp: Some(crate::types::DeviceTimestamp {
                ticks: leader.timestamp,
                frequency_hz: (self.timestamp_frequency != 0).then_some(self.timestamp_frequency),
            }),
            id: frame.block,
            width: leader.width,
            height,
            pixel_format: leader.pixel_format,
            device_ts_ns,
            wall_ms: 0,
            received_monotonic_ns: None,
            data: data.freeze(),
            jpeg: None,
            chunks: chunks.map(Into::into),
            multipart: None,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{MONO8, RGB8};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn ack(command: u16, id: u16, status: u16, data: &[u8]) -> Vec<u8> {
        let mut result = status.to_be_bytes().to_vec();
        result.extend(command.to_be_bytes());
        result.extend(u16::try_from(data.len()).unwrap().to_be_bytes());
        result.extend(id.to_be_bytes());
        result.extend(data);
        result
    }
    fn gvsp(frame: u64, id: u32, kind: u8, extended: bool, data: &[u8]) -> Vec<u8> {
        let mut result = vec![0, 0];
        if extended {
            result.extend([0, 0]);
            result.extend((0x8000_0000 | (u32::from(kind) << 24)).to_be_bytes());
            result.extend(frame.to_be_bytes());
            result.extend(id.to_be_bytes());
        } else {
            result.extend_from_slice(&frame.to_be_bytes()[6..]);
            result.extend(((u32::from(kind) << 24) | id).to_be_bytes());
        }
        result.extend(data);
        result
    }
    fn leader(width: u32, height: u32, pixel: u32) -> Vec<u8> {
        let mut result = vec![0, 0, 0, 1];
        result.extend(1234u64.to_be_bytes());
        result.extend(pixel.to_be_bytes());
        result.extend(width.to_be_bytes());
        result.extend(height.to_be_bytes());
        result.extend([0; 12]);
        result
    }
    fn trailer(height: u32) -> Vec<u8> {
        [1u32.to_be_bytes(), height.to_be_bytes()].concat()
    }
    fn image(width: u32, height: u32) -> Vec<u8> {
        (0..width * height)
            .map(|i| (i * 7).to_le_bytes()[0])
            .collect()
    }
    fn frame_packets(
        block: u64,
        extended: bool,
        width: u32,
        height: u32,
        block_size: usize,
    ) -> Vec<Vec<u8>> {
        let mut packets = vec![gvsp(block, 0, 1, extended, &leader(width, height, MONO8))];
        for chunk in image(width, height).chunks(block_size) {
            packets.push(gvsp(
                block,
                u32::try_from(packets.len()).unwrap(),
                3,
                extended,
                chunk,
            ));
        }
        packets.push(gvsp(
            block,
            u32::try_from(packets.len()).unwrap(),
            2,
            extended,
            &trailer(height),
        ));
        packets
    }
    fn resending(payload: usize) -> Assembler {
        let mut assembler = Assembler::new(payload, 44, 0);
        assembler.resend = true;
        assembler
    }

    /// Synthetic All-in IMAGE fixture, not a device capture.
    ///
    /// Wireshark cd59fea3baff8444a2645ba928e6967673c5b54a
    /// `dissect_packet_all_in`: 36-byte leader, 8-byte trailer, then image bytes.
    fn all_in_image(block: u64, extended: bool, id: u32, bytes: &[u8]) -> Vec<u8> {
        let body = [leader(2, 2, MONO8), trailer(2), bytes.to_vec()].concat();
        gvsp(block, id, 4, extended, &body)
    }

    #[test]
    fn all_in_literal_image_normalizes_padding_and_words_for_lossless_stills() {
        // The literal intentionally gives the common header a nonzero maximum
        // packet ID. This receiver never uses it as a DATA offset or count.
        // This is an independent synthetic layout fixture, not captured hardware.
        #[rustfmt::skip]
        const PACKET: [u8; 70] = [
            0x00,0x00,0x00,0x2a,0x04,0xff,0xff,0xff,
            0x00,0x00,0x00,0x01,0x00,0x00,0x00,0x00,
            0x00,0x00,0x04,0xd2,0x01,0x10,0x00,0x07,
            0x00,0x00,0x00,0x03,0x00,0x00,0x00,0x02,
            0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,
            0x00,0x02,0x00,0x02,
            0x00,0x00,0x00,0x01,0x00,0x00,0x00,0x02,
            0x01,0x02,0x03,0x04,0x05,0x06,0xfa,0xfb,
            0x07,0x08,0x09,0x0a,0x0b,0x0c,0xfc,0xfd,0xfe,0xff,
        ];
        let mut assembler = Assembler::new(128, 1500, 1_000_000);
        assembler.big_endian = true;
        let frame = assembler.push(&PACKET).unwrap().unwrap();
        let expected = [2, 1, 4, 3, 6, 5, 8, 7, 10, 9, 12, 11];
        assert_eq!(
            (frame.id, frame.width, frame.height, frame.pixel_format),
            (42, 3, 2, MONO16)
        );
        assert_eq!(frame.data.as_ref(), expected);
        assert_eq!(frame.device_ts_ns, Some(1_234_000));
        let clock = frame.device_timestamp.unwrap();
        assert_eq!((clock.ticks, clock.frequency_hz), (1234, Some(1_000_000)));
        assert!(frame.chunks.is_none() && frame.multipart.is_none());
        assert_eq!(frame.buffer_bytes(), frame.data.allocated_bytes());
        assert!(
            frame.buffer_bytes() >= 18,
            "the retained allocation includes wire padding"
        );
        assert_eq!(assembler.copied, (1, 18));
        assert!(assembler.frames.is_empty() && assembler.requests.is_empty());
        assert_eq!(crate::frame::encode(&frame, "raw").unwrap(), expected);
        let png = crate::frame::encode(&frame, "png").unwrap();
        let mut reader = png::Decoder::new(Cursor::new(png)).read_info().unwrap();
        let mut decoded = vec![0; reader.output_buffer_size()];
        let info = reader.next_frame(&mut decoded).unwrap();
        assert_eq!(
            (info.width, info.height, info.bit_depth),
            (3, 2, png::BitDepth::Sixteen)
        );
        assert_eq!(
            &decoded[..info.buffer_size()],
            &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]
        );
        // Later pool users cannot mutate an image retained by a saver or inspector.
        let held = frame.clone();
        drop(frame);
        for block in 43..48 {
            let next = assembler
                .push(&all_in_image(block, false, 0, &[99; 4]))
                .unwrap()
                .unwrap();
            assert_eq!(next.data.as_ref(), [99; 4]);
        }
        assert_eq!(held.data.as_ref(), expected);
    }

    #[test]
    fn all_in_documented_balluff_minimum_roi_fits_only_a_large_enough_datagram() {
        // Balluff X106C's published model list: Width min 376, Height min 4,
        // BayerRG8 and SCPS1500..8176. This is synthetic geometry evidence,
        // not a captured Balluff stream or a hardware qualification claim.
        let pixels = image(376, 4);
        let body = [leader(376, 4, 0x0108_0009), trailer(4), pixels.clone()].concat();
        for extended in [false, true] {
            let packet = gvsp(42, 0, 4, extended, &body);
            let mut too_small = Assembler::new(pixels.len(), 1500, 0);
            let error = too_small.push(&packet).unwrap_err();
            assert!(error.to_string().contains("negotiated packet size"));
            assert_eq!(too_small.copied, (0, 0));
            let mut exact = Assembler::new(pixels.len(), packet.len() + 28, 0);
            let frame = exact.push(&packet).unwrap().unwrap();
            assert_eq!(
                (frame.width, frame.height, frame.pixel_format),
                (376, 4, 0x0108_0009)
            );
            assert_eq!(frame.data.as_ref(), pixels);
            assert_eq!(exact.copied, (1, 1504));
        }
    }

    #[test]
    fn all_in_common_ids_never_size_packet_maps_or_generate_resends() {
        for extended in [false, true] {
            let mut assembler = Assembler::new(4, 1500, 0);
            assembler.resend = true;
            let block = if extended { 0x1_0000_002a } else { 42 };
            let id = if extended { u32::MAX } else { 0x00ff_ffff };
            let packet = all_in_image(block, extended, id, &[1, 2, 3, 4]);
            let frame = assembler.push(&packet).unwrap().unwrap();
            assert_eq!((frame.id, frame.data.as_ref()), (block, &[1, 2, 3, 4][..]));
            assert_eq!(frame.device_ts_ns, None);
            assert_eq!(frame.device_timestamp.unwrap().frequency_hz, None);
            assert!(assembler.push(&packet).unwrap().is_none());
            // A duplicate's packet ID is equally irrelevant after block delivery.
            assert!(
                assembler
                    .push(&all_in_image(block, extended, 0, &[9; 4]))
                    .unwrap()
                    .is_none()
            );
            assembler.poll(Instant::now() + FRAME_IDLE);
            assert!(
                assembler.frames.is_empty()
                    && assembler.ready.is_empty()
                    && assembler.requests.is_empty()
            );
            assert_eq!(
                (
                    assembler.stats.packets_received,
                    assembler.stats.resend_requested
                ),
                (3, 0)
            );
            assert_eq!(assembler.copied, (1, 4));
            assert_eq!(assembler.stats.incomplete_frames, 0);
        }
    }

    #[test]
    fn all_in_rejects_malformed_or_unsupported_frames_atomically_and_recovers() {
        for (mode, message) in [
            (0, "truncated GVSP All-in"),
            (1, "unchunked IMAGE"),
            (2, "unchunked IMAGE"),
            (3, "payload types disagree"),
            (4, "height 0"),
            (5, "height 3"),
            (6, "does not match image geometry"),
            (7, "does not match image geometry"),
            (8, "configured payload size"),
            (9, "negotiated packet size"),
            (10, "single image field"),
            (11, "configured payload size"),
        ] {
            let mut assembler = Assembler::new(
                if mode == 8 { 3 } else { 128 },
                if mode == 9 { 64 } else { 1500 },
                0,
            );
            let mut packet = all_in_image(42, false, 0, &[1, 2, 3, 4]);
            match mode {
                0 => packet.truncate(8 + 43),
                1 => packet[10..12].copy_from_slice(&5u16.to_be_bytes()),
                2 => packet[10..12].copy_from_slice(&0x4001u16.to_be_bytes()),
                3 => packet[46..48].copy_from_slice(&5u16.to_be_bytes()),
                4 => packet[48..52].fill(0),
                5 => packet[48..52].copy_from_slice(&3u32.to_be_bytes()),
                6 => {
                    packet.pop();
                }
                7 => packet.push(5),
                8 | 9 => {}
                10 => packet[8] = 0x11,
                11 => packet[24..28].copy_from_slice(&u32::MAX.to_be_bytes()),
                _ => unreachable!(),
            }
            let error = assembler.push(&packet).unwrap_err();
            assert!(
                error.to_string().contains(message),
                "mode {mode}: {error:#}"
            );
            assert!(assembler.frames.is_empty() && assembler.ready.is_empty());
            assert_eq!(assembler.copied, (0, 0));
            // Match the receiver's configured payload ceiling in the recovery image.
            let packets = frame_packets(43, false, 1, 2, if mode == 9 { 28 } else { 1464 });
            let frame = packets
                .iter()
                .find_map(|packet| assembler.push(packet).unwrap())
                .unwrap();
            assert_eq!((frame.id, frame.data.as_ref()), (43, &[0, 7][..]));
        }
    }

    #[test]
    fn all_in_accepts_only_an_exact_received_delivered_image_span() {
        let mut body = leader(2, 4, MONO8);
        body.extend(trailer(2));
        body.extend([1, 2, 3, 4]);
        let mut assembler = Assembler::new(8, 1500, 0);
        let frame = assembler
            .push(&gvsp(42, 0, 4, true, &body))
            .unwrap()
            .unwrap();
        assert_eq!((frame.width, frame.height), (2, 2));
        assert_eq!(frame.data.as_ref(), [1, 2, 3, 4]);
        assert_eq!(assembler.copied, (1, 4));
        assert!(frame.data.allocated_bytes() >= 4);
        // No leader-maximum tail is published when it was not in the datagram.
        let mut continuous = leader(5, 2, 0x010c_0047);
        continuous.extend(trailer(2));
        continuous.extend([
            0x21, 0x43, 0x65, 0x87, 0xa9, 0xcb, 0xed, 0x0f, 0x21, 0x43, 0x65, 0x87, 0xa9, 0xcb,
            0xed,
        ]);
        let mut assembler = Assembler::new(16, 1500, 0);
        let frame = assembler
            .push(&gvsp(43, 0, 4, false, &continuous))
            .unwrap()
            .unwrap();
        assert_eq!(
            frame.data.len(),
            16,
            "existing row normalization expands the exact 15-byte packed stream"
        );
        crate::frame::validate_source(&frame).unwrap();
        let mut big = Assembler::new(16, 1500, 0);
        big.big_endian = true;
        continuous[12..16].copy_from_slice(&crate::frame::COORD3D_C12P.to_be_bytes());
        let error = big.push(&gvsp(44, 0, 4, false, &continuous)).unwrap_err();
        assert_eq!(
            crate::types::classify(&error).kind,
            crate::types::ErrorKind::Unsupported
        );
        assert!(error.to_string().contains("little-endian packing"));
        assert_eq!(big.copied, (0, 0));
    }

    #[test]
    fn all_in_batches_reconstruct_before_replacing_an_ordinary_receive_owner() {
        let mut packets = frame_packets(41, true, 4, 2, 16);
        packets.truncate(1);
        packets.push(all_in_image(42, true, u32::MAX, &[1, 2, 3, 4]));
        packets.extend(frame_packets(41, true, 4, 2, 16).into_iter().skip(1));
        packets.extend(frame_packets(43, true, 2, 2, 16));
        packets.push(all_in_image(44, true, 0, &[5, 6, 7, 8]));
        let expected = vec![
            (42, vec![1, 2, 3, 4]),
            (43, image(2, 2)),
            (44, vec![5, 6, 7, 8]),
        ];
        for batch_size in [None, Some(0_usize), Some(1), Some(3), Some(31)] {
            let mut assembler = Assembler::new(8, 1500, 0);
            let mut random = |_: usize| batch_size.unwrap_or_default();
            let frames = assembled(
                &mut assembler,
                &packets,
                batch_size.map(|_| &mut random as &mut dyn FnMut(usize) -> usize),
            );
            assert_eq!(frames, expected, "batch size {batch_size:?}");
            assert_eq!(assembler.stats.incomplete_frames, 1);
            assert!(assembler.frames.is_empty());
        }
        let mut assembler = Assembler::new(8, 1500, 0);
        assembler
            .push(&gvsp(42, 0, 1, true, &leader(2, 2, MONO8)))
            .unwrap();
        let error = assembler
            .push(&all_in_image(42, true, 0, &[1, 2, 3, 4]))
            .unwrap_err();
        assert!(error.to_string().contains("mixes All-in and separate"));
        assert!(assembler.frames.is_empty() && assembler.ready.is_empty());
    }

    #[test]
    fn all_in_truncated_receive_never_publishes_a_clipped_prefix() {
        let packet = all_in_image(42, true, 0, &[1, 2, 3, 4]);
        for predicted in [false, true] {
            let mut assembler = Assembler::new(8, 1500, 0);
            if predicted {
                assembler
                    .push(&gvsp(41, 0, 1, true, &leader(4, 2, MONO8)))
                    .unwrap();
            }
            let mut batch = batch::Batch::new(1500);
            let slots: Vec<_> = assembler.predict_run(1).collect();
            let base = if predicted {
                assembler.newest_buffer().unwrap()
            } else {
                std::ptr::null_mut()
            };
            if let Some(slot) = slots.first() {
                batch.headers[0][..slot.header].copy_from_slice(&packet[..slot.header]);
                let rest = &packet[slot.header..];
                let in_slot = rest.len().min(slot.len);
                assembler.slot_mut(slot).unwrap()[..in_slot].copy_from_slice(&rest[..in_slot]);
                batch.rests[0][..rest.len() - in_slot].copy_from_slice(&rest[in_slot..]);
            } else {
                batch.rests[0][..packet.len()].copy_from_slice(&packet);
            }
            assert!(
                batch
                    .take(
                        &mut assembler,
                        (&slots, base),
                        &[batch::Received {
                            len: packet.len(),
                            truncated: true,
                            from_camera: true
                        }],
                        Site::GvspFallback,
                        Instant::now()
                    )
                    .is_none()
            );
            assert!(assembler.ready.is_empty());
            assert_eq!(assembler.copied, (0, 0));
            let frame = assembler.push(&packet).unwrap().unwrap();
            assert_eq!(frame.data.as_ref(), [1, 2, 3, 4]);
        }
    }

    #[test]
    fn all_in_fake_camera_stream_captures_frames_without_packet_resends() {
        for extended in [false, true] {
            let (fake, mut gige) = streaming(|camera| {
                camera.registers.insert(GVCP_CAPABILITY, 0x4);
            });
            let destination = fake.camera.lock().unwrap().destination();
            let first = all_in_image(
                42,
                extended,
                if extended { u32::MAX } else { 0x00ff_ffff },
                &[1, 2, 3, 4],
            );
            fake.stream.send_to(&first, destination).unwrap();
            let frame = gige.next_frame(Duration::from_secs(2)).unwrap();
            assert_eq!((frame.id, frame.width, frame.height), (42, 2, 2));
            assert_eq!(frame.data.as_ref(), [1, 2, 3, 4]);
            fake.stream.send_to(&first, destination).unwrap();
            fake.stream
                .send_to(&all_in_image(43, extended, 0, &[5, 6, 7, 8]), destination)
                .unwrap();
            let second = gige.next_frame(Duration::from_secs(2)).unwrap();
            assert_eq!(second.id, 43);
            assert_eq!(second.data.as_ref(), [5, 6, 7, 8]);
            assert_eq!(frame.data.as_ref(), [1, 2, 3, 4]);
            let raw = crate::frame::encode_parts(&frame, "raw", None, None).unwrap();
            assert_eq!(
                raw.parts()[0].as_ptr(),
                frame.data.as_ptr(),
                "RAW capture borrows the received owner"
            );
            let dir = tempfile::tempdir().unwrap();
            let output = dir.path().join("captured.raw");
            let stored = crate::storage::save_parts(
                &raw.parts(),
                &output,
                crate::storage::SyncMode::Off,
                Some(0),
                true,
            )
            .unwrap();
            assert_eq!(stored.bytes, 4);
            assert!(stored.sha256.is_some());
            assert_eq!(std::fs::read(&output).unwrap(), [1, 2, 3, 4]);
            let error = crate::storage::save_parts(
                &raw.parts(),
                &output,
                crate::storage::SyncMode::Off,
                Some(0),
                false,
            )
            .unwrap_err();
            assert!(
                error
                    .downcast_ref::<crate::storage::OutputExists>()
                    .is_some()
            );
            assert_eq!(std::fs::read(&output).unwrap(), [1, 2, 3, 4]);
            assert_eq!(fake.camera.lock().unwrap().requests, Vec::new());
            let stats = gige.stats().unwrap();
            assert_eq!((stats.resend_requested, stats.incomplete_frames), (0, 0));
            gige.stop().unwrap();
            assert_eq!(fake.register(STREAM_PORT), 0);
            drop(gige);
            assert_eq!(fake.register(CONTROL_PRIVILEGE), 0);
        }
    }

    // Independently authored wire fixture with unequal part dimensions.
    // Packet order is deliberately different from component order.
    fn multipart_packets(block: u64, data_size: usize) -> (Vec<Vec<u8>>, [Vec<u8>; 4]) {
        multipart_packets_with_confidence(block, data_size, false)
    }

    fn multipart_packets_with_confidence(
        block: u64,
        data_size: usize,
        wide_confidence: bool,
    ) -> (Vec<Vec<u8>>, [Vec<u8>; 4]) {
        let confidence = if wide_confidence {
            0x0110_00c7
        } else {
            0x0108_00c6
        };
        let confidence_bytes = if wide_confidence { 646 } else { 323 };
        let descriptors = [
            (5u16, 0x0110_00b8u32, 17u32, 19u32),
            (1, MONO8, 33, 21),
            (9, confidence, 17, 19),
            (1, 0x8108_0001, 17, 19),
        ];
        let parts = [
            (0..646u32)
                .map(|i| (i * 3).to_le_bytes()[0])
                .collect::<Vec<_>>(),
            image(33, 21),
            (0..confidence_bytes)
                .map(|i: u32| (i * 5).to_le_bytes()[0])
                .collect(),
            (0..323u32).map(|i| (i * 11).to_le_bytes()[0]).collect(),
        ];
        let packets = multipart_wire_packets(block, data_size, &descriptors, &parts, &[1, 0, 3, 2]);
        (packets, parts)
    }

    fn multipart_wire_packets(
        block: u64,
        data_size: usize,
        descriptors: &[(u16, u32, u32, u32)],
        parts: &[Vec<u8>],
        order: &[usize],
    ) -> Vec<Vec<u8>> {
        let mut head = vec![0, 0, 0, 10];
        head.extend(9876u64.to_be_bytes());
        let mut tail = vec![0, 0, 0, 10];
        for (index, ((kind, format, width, height), samples)) in
            descriptors.iter().copied().zip(parts).enumerate()
        {
            let length = u64::try_from(samples.len()).unwrap().to_be_bytes();
            head.extend(kind.to_be_bytes());
            head.extend_from_slice(&length[2..]);
            head.extend(format.to_be_bytes());
            head.extend([0, 0, u8::try_from(index).unwrap(), 0]);
            head.extend(0u32.to_be_bytes());
            head.extend(1u16.to_be_bytes());
            head.extend(3u16.to_be_bytes());
            head.extend(width.to_be_bytes());
            head.extend(height.to_be_bytes());
            head.extend([0; 16]);
            tail.extend(kind.to_be_bytes());
            tail.extend_from_slice(&length[2..]);
            tail.extend(height.to_be_bytes());
            tail.extend([0; 4]);
        }
        let mut leader = gvsp(block, 0, 1, true, &head);
        leader[7] = u8::try_from(parts.len()).unwrap();
        let mut packets = vec![leader];
        for &part in order {
            let base: usize = parts[..part].iter().map(Vec::len).sum();
            for (index, fragment) in parts[part].chunks(data_size).enumerate() {
                let end = index * data_size + fragment.len() == parts[part].len();
                let mut data = vec![u8::try_from(part).unwrap(), if end { 0x41 } else { 0 }];
                let address = u64::try_from(base + index * data_size)
                    .unwrap()
                    .to_be_bytes();
                data.extend_from_slice(&address[2..]);
                data.extend_from_slice(fragment);
                packets.push(gvsp(
                    block,
                    u32::try_from(packets.len()).unwrap(),
                    7,
                    true,
                    &data,
                ));
            }
        }
        packets.push(gvsp(
            block,
            u32::try_from(packets.len()).unwrap(),
            2,
            true,
            &tail,
        ));
        packets
    }

    fn assert_multipart_capture(frame: &Frame, parts: &[Vec<u8>; 4]) {
        assert_eq!(
            (frame.width, frame.height, frame.pixel_format),
            (33, 21, MONO8)
        );
        assert_eq!(frame.data.as_ref(), parts[1]);
        assert_eq!(frame.device_timestamp.unwrap().ticks, 9876);
        assert!(frame.chunks.is_none());
        let archive = frame.multipart.as_ref().unwrap();
        assert_eq!(archive.payload.as_ref(), parts.concat());
        assert_eq!(archive.layout.selected, 1);
        let layout = serde_json::to_value(&archive.layout).unwrap();
        assert_eq!(layout["data_type_namespace"], "gvsp");
        assert!(layout.get("original_buffer_bytes").is_none());
        assert_eq!(layout["original_filled_bytes"], parts.concat().len());
        let mut address = 0;
        for (index, (part, samples)) in layout["parts"]
            .as_array()
            .unwrap()
            .iter()
            .zip(parts)
            .enumerate()
        {
            assert_eq!(part["index"], index);
            assert_eq!(part["stored_offset"], address);
            assert_eq!(part["gvsp"]["address_offset"], address);
            assert_eq!(part["length"], samples.len());
            assert_eq!(part["data_type"], [5, 1, 9, 1][index]);
            assert_eq!(
                part["format"],
                [0x0110_00b8, MONO8, 0x0108_00c6, 0x8108_0001][index]
            );
            assert!(part.get("original_offset").is_none());
            assert!(part.get("namespace").is_none());
            assert_eq!(part["source_id"], index);
            assert_eq!(part["purpose_id"], 1);
            assert_eq!(part["region_id"], 3);
            assert_eq!(part["gvsp"]["x_padding"], 0);
            address += samples.len();
        }
    }

    #[test]
    fn exact_index_admission_requires_all_packets_and_never_exposes_an_ordinary_representative() {
        use crate::transport::multipart::GvspPartFormat;
        for format in [MONO8, RGB8] {
            let component = if format == MONO8 { 1 } else { 3 };
            let parts = [
                [10, 20].repeat(component),
                [30, 40, 50, 60].repeat(component),
                vec![0xab],
            ];
            let descriptors = [(1, format, 2, 1), (1, format, 2, 2), (1, 0x8108_0001, 1, 1)];
            let size = parts.concat().len();
            let mut packets =
                multipart_wire_packets(117, 2 * component, &descriptors, &parts, &[1, 0, 2]);
            // Purpose/source/region are deliberately distinct; ordinal selection never aliases them.
            packets[0][46] = 68;
            packets[0][94] = 68;
            packets[0][100..102].copy_from_slice(&65_281u16.to_be_bytes());
            let mut closed = Assembler::new(size, 576, 0);
            let error = closed.push(&packets[0]).unwrap_err();
            assert_eq!(
                crate::types::classify(&error).kind,
                crate::types::ErrorKind::Unsupported
            );
            let start = Instant::now();
            let mut assembler = Assembler::new(size, 576, 0);
            assembler.multipart_projection = true;
            assembler.resend = true;
            for index in [5, 2, 0, 4, 1] {
                assert!(assembler.push_at(&packets[index], start).unwrap().is_none());
            }
            assembler.poll(start + RESEND_IDLE);
            assert!(
                assembler
                    .requests
                    .iter()
                    .any(|&(block, _, first, last)| block == 117 && first <= 3 && last >= 3)
            );
            let recovered = assembler.stats.resend_recovered;
            let mut resent = packets[3].clone();
            resent[..2].copy_from_slice(&0x0100u16.to_be_bytes());
            let frame = assembler
                .push_at(&resent, start + RESEND_IDLE)
                .unwrap()
                .unwrap();
            assert_eq!(assembler.stats.resend_recovered, recovered + 1);
            assert!(crate::frame::selection_required(&frame));
            crate::frame::rgb(&frame).unwrap_err();
            serde_json::to_value(&frame).unwrap_err();
            let selection = if format == MONO8 {
                GvspPartFormat::Mono8
            } else {
                GvspPartFormat::Rgb8
            };
            selection.project(&frame).unwrap_err();
            let selected = selection.project_index(&frame, Some(1)).unwrap();
            assert_eq!(selected.data.as_ref(), parts[1]);
            assert_eq!(
                selected.multipart.as_ref().unwrap().payload.as_ref(),
                parts.concat()
            );
            assert_eq!(
                selected.multipart.as_ref().unwrap().layout.parts[1].purpose_id,
                Some(65_281)
            );
            assert_eq!((selected.width, selected.height), (2, 2));
        }
    }

    #[test]
    fn duplicate_scalar_native_parts_require_selection_before_inspect_or_frame_serde() {
        use crate::transport::multipart::GvspPartFormat;
        let parts = [vec![0x12, 0x34], vec![0x56, 0x78]];
        let packets = multipart_wire_packets(
            118,
            8,
            &[(5, 0x0110_00b8, 1, 1), (5, 0x0110_00b8, 1, 1)],
            &parts,
            &[0, 1],
        );
        let mut assembler = Assembler::new(4, 576, 0);
        let frame = packets
            .iter()
            .find_map(|packet| assembler.push(packet).unwrap())
            .unwrap();
        assert!(crate::frame::selection_required(&frame));
        serde_json::to_value(&frame).unwrap_err();
        crate::inspect::inspect(
            &frame,
            crate::camera_profile::CameraKind::AreaScan,
            None,
            0,
            crate::inspect::PreviewFormat::Png,
        )
        .unwrap_err();
        GvspPartFormat::Coord3dC16.project(&frame).unwrap_err();
        let selected = GvspPartFormat::Coord3dC16
            .project_index(&frame, Some(1))
            .unwrap();
        assert_eq!(selected.data.as_ref(), parts[1]);
        assert!(!crate::frame::selection_required(&selected));
    }

    #[test]
    fn multipart_wire_packed_range_keeps_payload_and_png_sample_depth() {
        use crate::frame::{COORD3D_C10P, COORD3D_C12P};
        use crate::transport::multipart::GvspPartFormat;
        // PFNC LSB bytes computed from these sample codes independently of
        // the row decoder. Two rows expose any accidental stride changes.
        let cases = [
            (
                GvspPartFormat::Coord3dC10p,
                COORD3D_C10P,
                4u32,
                10u8,
                [0x00, 0x04, 0x50, 0xd5, 0xff, 0x01, 0x8e, 0xa4, 0xaa, 0xff].as_slice(),
                [0u16, 1, 0x155, 0x3ff, 0x201, 0x123, 0x2aa, 0x3fe].as_slice(),
            ),
            (
                GvspPartFormat::Coord3dC12p,
                COORD3D_C12P,
                2,
                12,
                [0xbc, 0x3a, 0x12, 0x01, 0xf0, 0xff].as_slice(),
                [0xabcu16, 0x123, 1, 0xfff].as_slice(),
            ),
        ];
        for (format, code, width, bits, packed, samples) in cases {
            for reflectance in [true, false] {
                let parts = [packed.to_vec(), vec![42, 43]];
                let descriptors = [(5, code, width, 2), (1, MONO8, 1, 2)];
                let count = if reflectance { 2 } else { 1 };
                let packets = multipart_wire_packets(
                    55,
                    520,
                    &descriptors[..count],
                    &parts[..count],
                    &[0, 1][..count],
                );
                let mut assembler = Assembler::new(12, 576, 0);
                let frame = packets
                    .iter()
                    .find_map(|packet| assembler.push(packet).unwrap())
                    .unwrap();
                assert_eq!(frame.pixel_format, if reflectance { MONO8 } else { code });
                let selected = format.project(&frame).unwrap();
                assert_eq!(
                    (selected.width, selected.height, selected.pixel_format),
                    (width, 2, code)
                );
                assert_eq!(crate::frame::encode(&selected, "raw").unwrap(), packed);
                let archive = selected.multipart.as_ref().unwrap();
                assert_eq!(archive.layout.selected, 0);
                assert_eq!(archive.payload.as_ref(), parts[..count].concat());
                assert!(crate::auto::meter(&selected).is_none());

                let png = crate::frame::encode(&selected, "png").unwrap();
                let mut reader = png::Decoder::new(png.as_slice()).read_info().unwrap();
                let mut bytes = vec![0; reader.output_buffer_size()];
                let image = reader.next_frame(&mut bytes).unwrap();
                assert_eq!(image.bit_depth, png::BitDepth::Sixteen);
                assert_eq!(image.color_type, png::ColorType::Grayscale);
                assert_eq!(reader.info().sbit.as_deref(), Some([bits].as_slice()));
                let expected: Vec<u8> = samples
                    .iter()
                    .flat_map(|sample| (sample << (16 - bits)).to_be_bytes())
                    .collect();
                assert_eq!(&bytes[..image.buffer_size()], expected);
                let gray: Vec<u8> = samples
                    .iter()
                    .map(|sample| u8::try_from(sample >> (bits - 8)).unwrap())
                    .collect();
                assert_eq!(crate::frame::gray8(&selected).unwrap().unwrap(), gray);
                assert_eq!(crate::frame::sample_bits(code), Some(u32::from(bits)));
            }
        }
    }

    #[test]
    fn multipart_wire_packed_range_rejects_unaligned_or_unconfirmed_packing() {
        use crate::frame::{COORD3D_C10P, COORD3D_C12P};
        for (code, width, payload, big, unknown, expected) in [
            (COORD3D_C10P, 3, 4, false, false, "byte-aligned rows"),
            (COORD3D_C12P, 1, 2, false, false, "byte-aligned rows"),
            (COORD3D_C12P, 2, 3, true, false, "little-endian packing"),
            (COORD3D_C10P, 4, 5, false, true, "little-endian packing"),
        ] {
            let packets =
                multipart_wire_packets(56, 520, &[(5, code, width, 1)], &[vec![0; payload]], &[0]);
            let mut assembler = Assembler::new(payload, 576, 0);
            assembler.big_endian = big;
            assembler.big_endian_mono16 = unknown;
            let error = assembler.push(&packets[0]).unwrap_err();
            assert_eq!(
                crate::types::error_transport_code(&error),
                Some("unsupported")
            );
            assert!(error.to_string().contains(expected), "{error:#}");
        }
    }

    #[test]
    fn ordinary_wire_packed_range_uses_the_same_typed_samples() {
        use crate::frame::COORD3D_C12P;
        let mut assembler = Assembler::new(6, 576, 0);
        let header = gvsp(57, 0, 1, true, &leader(2, 2, COORD3D_C12P));
        assert!(assembler.push(&header).unwrap().is_none());
        let pixels = [0xbc, 0x3a, 0x12, 0x01, 0xf0, 0xff];
        assert!(
            assembler
                .push(&gvsp(57, 1, 3, true, &pixels))
                .unwrap()
                .is_none()
        );
        let frame = assembler
            .push(&gvsp(57, 2, 2, true, &trailer(2)))
            .unwrap()
            .unwrap();
        assert_eq!(frame.data.as_ref(), pixels);
        assert_eq!(frame.pixel_format, COORD3D_C12P);
        assert_eq!(
            crate::frame::gray8(&frame).unwrap().unwrap(),
            [0xab, 0x12, 0, 0xff]
        );
        assert!(crate::auto::meter(&frame).is_none());
        assembler.big_endian = true;
        let error = assembler
            .push(&gvsp(58, 0, 1, true, &leader(2, 2, COORD3D_C12P)))
            .unwrap_err();
        assert_eq!(
            crate::types::error_transport_code(&error),
            Some("unsupported")
        );
    }

    #[test]
    fn multipart_wire_abc16_preserves_each_unsigned_word_and_original_components() {
        use crate::frame::COORD3D_ABC16;
        use crate::transport::multipart::GvspPartFormat;
        let words = [0, 1, u16::MAX, 0x8000, 0xfffe, 0x1234];
        let normalized: Vec<_> = words.into_iter().flat_map(u16::to_le_bytes).collect();
        for (big_endian, cloud_first) in [(false, false), (true, false), (true, true)] {
            let cloud: Vec<_> = if big_endian {
                words.into_iter().flat_map(u16::to_be_bytes).collect()
            } else {
                normalized.clone()
            };
            let mono = if big_endian {
                vec![0x12, 0x34]
            } else {
                vec![0x34, 0x12]
            };
            let mut parts = [mono, cloud];
            let mut descriptors = [(1, MONO16, 1, 1), (5, COORD3D_ABC16, 2, 1)];
            if cloud_first {
                parts.swap(0, 1);
                descriptors.swap(0, 1);
            }
            let packets = multipart_wire_packets(59, 520, &descriptors, &parts, &[1, 0]);
            let mut assembler = Assembler::new(14, 576, 0);
            assembler.big_endian = big_endian;
            let frame = packets
                .iter()
                .find_map(|packet| assembler.push(packet).unwrap())
                .unwrap();
            let selected = GvspPartFormat::Coord3dAbc16.project(&frame).unwrap();
            assert_eq!(
                (selected.width, selected.height, selected.pixel_format),
                (2, 1, COORD3D_ABC16)
            );
            assert_eq!(selected.data.as_ref(), normalized);
            let archive = selected.multipart.as_ref().unwrap();
            assert_eq!(archive.layout.selected, u32::from(!cloud_first));
            assert_eq!(archive.payload.as_ref(), parts.concat());
            assert_eq!(
                archive.payload.as_ptr(),
                frame.multipart.as_ref().unwrap().payload.as_ptr()
            );
            if cloud_first {
                assert_eq!(selected.data.as_ptr(), frame.data.as_ptr());
            }
            assert!(crate::auto::meter(&selected).is_none());
        }
    }

    #[test]
    fn qualified_lucid_abcy16_keeps_little_endian_words_and_big_endian_stays_opaque() {
        use crate::types::LucidPixelFormat;
        const ABCY16: u32 = 0x8240_0403;
        // Two pixels of asymmetric A, B, C, Y words, each low byte first on the wire.
        let wire = [
            0x34, 0x12, 0x00, 0x80, 0xfe, 0xff, 0x01, 0x00, //
            0xff, 0xff, 0x00, 0x00, 0xcd, 0xab, 0x00, 0x7f,
        ];
        for (big_endian, qualified, tagged) in [
            (false, true, true),
            (false, false, false),
            (true, true, false),
        ] {
            let mut assembler = Assembler::new(16, 576, 0);
            assembler.big_endian = big_endian;
            if qualified {
                assembler.pixel_encodings = vec![LucidPixelFormat::Coord3dAbcy16.encoding(None)];
            }
            assert!(
                assembler
                    .push(&gvsp(61, 0, 1, true, &leader(2, 1, ABCY16)))
                    .unwrap()
                    .is_none()
            );
            assert!(
                assembler
                    .push(&gvsp(61, 1, 3, true, &wire))
                    .unwrap()
                    .is_none()
            );
            let frame = assembler
                .push(&gvsp(61, 2, 2, true, &trailer(1)))
                .unwrap()
                .unwrap();
            // No swap: neither a qualified nor an opaque custom code is reinterpreted.
            assert_eq!(frame.data.as_ref(), wire.as_slice());
            assert_eq!(frame.pixel_format, ABCY16);
            assert_eq!(
                frame.pixel_encoding,
                tagged.then(|| LucidPixelFormat::Coord3dAbcy16.encoding(None))
            );
            assert_eq!(crate::frame::is_measurement_frame(&frame), tagged);
            if tagged {
                assert_eq!(crate::frame::frame_format_name(&frame), "Coord3D_ABCY16");
                assert!(crate::auto::meter(&frame).is_none());
                crate::frame::encode_parts(&frame, "raw", None, None).unwrap();
                for format in ["png", "jpeg"] {
                    crate::frame::encode_parts(&frame, format, None, None).unwrap_err();
                }
            }
        }
    }

    #[test]
    fn ordinary_wire_abc16_swaps_two_byte_words_without_interpreting_codes() {
        use crate::frame::COORD3D_ABC16;
        let words = [0, 1, u16::MAX, 0x8000, 0xfffe, 0x1234];
        let expected: Vec<_> = words.into_iter().flat_map(u16::to_le_bytes).collect();
        let mut assembler = Assembler::new(12, 576, 0);
        assembler.big_endian = true;
        let wire: Vec<_> = words.into_iter().flat_map(u16::to_be_bytes).collect();
        assert!(
            assembler
                .push(&gvsp(60, 0, 1, true, &leader(2, 1, COORD3D_ABC16)))
                .unwrap()
                .is_none()
        );
        assert!(
            assembler
                .push(&gvsp(60, 1, 3, true, &wire))
                .unwrap()
                .is_none()
        );
        let frame = assembler
            .push(&gvsp(60, 2, 2, true, &trailer(1)))
            .unwrap()
            .unwrap();
        assert_eq!(frame.data.as_ref(), expected);
        assert_eq!(frame.pixel_format, COORD3D_ABC16);
        assert!(frame.multipart.is_none());
        assert!(crate::auto::meter(&frame).is_none());
    }

    #[test]
    fn multipart_wire_abc32f_preserves_each_word_and_original_components() {
        use crate::frame::COORD3D_ABC32F;
        use crate::transport::multipart::GvspPartFormat;
        let words = [
            0x3f80_0001u32,
            0x8000_0000,
            0x7fc0_1234,
            0xbf80_0001,
            0xff80_0000,
            0x0000_0000,
        ];
        let normalized: Vec<_> = words.into_iter().flat_map(u32::to_le_bytes).collect();
        for (big_endian, cloud_first) in [(false, false), (true, false), (true, true)] {
            let cloud: Vec<_> = if big_endian {
                words.into_iter().flat_map(u32::to_be_bytes).collect()
            } else {
                normalized.clone()
            };
            let mono = if big_endian {
                vec![0x12, 0x34]
            } else {
                vec![0x34, 0x12]
            };
            let mut parts = [mono, cloud];
            let mut descriptors = [(1, MONO16, 1, 1), (5, COORD3D_ABC32F, 2, 1)];
            if cloud_first {
                parts.swap(0, 1);
                descriptors.swap(0, 1);
            }
            let packets = multipart_wire_packets(53, 520, &descriptors, &parts, &[1, 0]);
            let mut assembler = Assembler::new(26, 576, 0);
            assembler.big_endian = big_endian;
            let frame = packets
                .iter()
                .find_map(|packet| assembler.push(packet).unwrap())
                .unwrap();
            assert_eq!(frame.pixel_format, descriptors[0].1);
            let selected = GvspPartFormat::Coord3dAbc32f.project(&frame).unwrap();
            assert_eq!(
                (selected.width, selected.height, selected.pixel_format),
                (2, 1, COORD3D_ABC32F)
            );
            assert_eq!(selected.data.as_ref(), normalized);
            let archive = selected.multipart.as_ref().unwrap();
            assert_eq!(archive.layout.selected, u32::from(!cloud_first));
            assert_eq!(archive.payload.as_ref(), parts.concat());
            assert_eq!(
                archive.payload.as_ptr(),
                frame.multipart.as_ref().unwrap().payload.as_ptr()
            );
            if cloud_first {
                assert_eq!(selected.data.as_ptr(), frame.data.as_ptr());
            }
            assert!(crate::auto::meter(&selected).is_none());
        }
    }

    #[test]
    fn multipart_wire_abc32f_refuses_wrong_type_length_and_duplicates() {
        use crate::frame::COORD3D_ABC32F;
        use crate::transport::multipart::GvspPartFormat;
        for (kind, size, count, expected) in [
            (6, 12, 2, "required wire type"),
            (5, 11, 2, "exact geometry"),
            (5, 12, 3, "multiple Coord3D_ABC32f parts"),
        ] {
            let descriptors = [
                (1, MONO16, 1, 1),
                (kind, COORD3D_ABC32F, 1, 1),
                (5, COORD3D_ABC32F, 1, 1),
            ];
            let parts = [vec![0x34, 0x12], vec![0x89; size], vec![0x45; 12]];
            let packets = multipart_wire_packets(
                54,
                520,
                &descriptors[..count],
                &parts[..count],
                &[0, 1, 2][..count],
            );
            let mut assembler = Assembler::new(26, 576, 0);
            let frame = packets
                .iter()
                .find_map(|packet| assembler.push(packet).unwrap())
                .unwrap();
            assert_eq!(
                frame.multipart.as_ref().unwrap().payload.as_ref(),
                parts[..count].concat()
            );
            let error = GvspPartFormat::Coord3dAbc32f.project(&frame).unwrap_err();
            assert!(error.to_string().contains(expected), "{error:#}");
        }
    }

    #[test]
    fn multipart_wire_neutral_mono16_preserves_samples_and_purpose_ids() {
        use crate::transport::multipart::GvspPartFormat;
        let little = [0x34, 0x12, 0xff, 0xff];
        let big = [0x12, 0x34, 0xff, 0xff];
        for (purpose, big_endian, with_intensity) in [
            (0u16, false, true),
            (1, true, false),
            (4, false, false),
            (6, true, false),
        ] {
            let parts = [
                if big_endian { big } else { little }.to_vec(),
                vec![0x80, 0xff],
                vec![7],
            ];
            let descriptors = [(1, MONO16, 2, 1), (9, 0x0108_00c6, 2, 1), (1, MONO8, 1, 1)];
            let count = if with_intensity { 3 } else { 2 };
            let mut packets = multipart_wire_packets(
                50,
                520,
                &descriptors[..count],
                &parts[..count],
                &[1, 0, 2][..count],
            );
            packets[0][52..54].copy_from_slice(&purpose.to_be_bytes());
            let mut assembler = Assembler::new(7, 576, 0);
            assembler.big_endian = big_endian;
            let frame = packets
                .iter()
                .find_map(|packet| assembler.push(packet).unwrap())
                .unwrap();
            let archive = frame.multipart.as_ref().unwrap();
            assert_eq!(archive.layout.selected, if with_intensity { 2 } else { 0 });
            assert_eq!(archive.layout.parts[0].purpose_id, Some(u64::from(purpose)));
            assert_eq!(archive.payload.as_ref(), parts[..count].concat());
            let selected = GvspPartFormat::Mono16.project(&frame).unwrap();
            let saved = selected.multipart.as_ref().unwrap();
            assert_eq!(saved.layout.selected, 0);
            assert_eq!(saved.payload.as_ptr(), archive.payload.as_ptr());
            assert_eq!(
                saved.layout.byte_order,
                Some(if big_endian {
                    ByteOrder::Big
                } else {
                    ByteOrder::Little
                })
            );
            assert_eq!(selected.data.as_ref(), little);
            assert_eq!(selected.pixel_format, MONO16);
            if !with_intensity {
                assert_eq!(selected.data.as_ptr(), frame.data.as_ptr());
            }
            assert!(crate::auto::meter(&selected).is_none());
            let inspection = crate::inspect::inspect(
                &selected,
                crate::camera_profile::CameraKind::AreaScan,
                None,
                crate::inspect::default_edge(),
                crate::inspect::PreviewFormat::Png,
            )
            .unwrap();
            assert!(inspection.measurements.exposure.is_none());
            assert!(inspection.measurements.focus.is_none());
            assert!(inspection.measurements.notes[0].contains("purpose is unspecified"));

            let png = crate::frame::encode(&selected, "png").unwrap();
            let mut reader = png::Decoder::new(Cursor::new(png)).read_info().unwrap();
            let mut bytes = vec![0; reader.output_buffer_size()];
            let image = reader.next_frame(&mut bytes).unwrap();
            assert_eq!(image.bit_depth, png::BitDepth::Sixteen);
            assert_eq!(image.color_type, png::ColorType::Grayscale);
            assert_eq!(&bytes[..image.buffer_size()], &big);
        }
    }

    #[test]
    fn multipart_wire_neutral_mono16_requires_one_exact_2d_part() {
        use crate::transport::multipart::GvspPartFormat;
        let parts = [vec![0x34, 0x12], vec![0xff, 0xff], vec![7]];
        for (kinds, count, expected) in [
            ([5, 9], 3, "required wire type"),
            ([1, 1], 2, "multiple Mono16 parts"),
        ] {
            let descriptors = [
                (kinds[0], MONO16, 1, 1),
                (kinds[1], MONO16, 1, 1),
                (1, MONO8, 1, 1),
            ];
            let mut packets = multipart_wire_packets(
                51,
                520,
                &descriptors[..count],
                &parts[..count],
                &[0, 1, 2][..count],
            );
            packets[0][52..54].copy_from_slice(&4u16.to_be_bytes());
            packets[0][100..102].copy_from_slice(&6u16.to_be_bytes());
            let mut assembler = Assembler::new(5, 576, 0);
            let frame = packets
                .iter()
                .find_map(|packet| assembler.push(packet).unwrap())
                .unwrap();
            if count == 3 {
                assert_eq!(frame.data.as_ref(), &[7]);
            }
            assert_eq!(
                frame.multipart.as_ref().unwrap().payload.as_ref(),
                parts[..count].concat()
            );
            let error = GvspPartFormat::Mono16.project(&frame).unwrap_err();
            assert_eq!(
                crate::types::error_transport_code(&error),
                Some("unsupported")
            );
            assert!(error.to_string().contains(expected), "{error:#}");
        }

        let packets = multipart_wire_packets(52, 520, &[(1, MONO16, 1, 1)], &parts[..1], &[0]);
        let mut assembler = Assembler::new(2, 576, 0);
        assembler.big_endian_mono16 = true;
        let error = assembler.push(&packets[0]).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("sample byte order is unavailable")
        );
    }

    #[test]
    fn multipart_wire_keeps_measurement_only_blocks_and_reuses_the_representative() {
        use crate::transport::multipart::GvspPartFormat;
        let parts = [
            vec![0x12, 0x34, 0xff, 0xff],
            vec![0x80, 0xff],
            vec![0x43, 0x21, 0xaa],
        ];
        let packets = multipart_wire_packets(
            47,
            520,
            &[
                (5, 0x0110_00b8, 2, 1),
                (9, 0x0108_00c6, 2, 1),
                (0x8000, 0, 0, 0),
            ],
            &parts,
            &[1, 0, 2],
        );
        let mut assembler = Assembler::new(9, 576, 1_000_000);
        assembler.big_endian = true;
        for index in [1, 3, 0, 2] {
            assert!(assembler.push(&packets[index]).unwrap().is_none());
        }
        let frame = assembler.push(&packets[4]).unwrap().unwrap();
        let archive = frame.multipart.as_ref().unwrap();
        assert_eq!(
            archive.layout.selected_native_part_format(),
            Some(GvspPartFormat::Coord3dC16)
        );
        assert_eq!(archive.layout.byte_order, Some(ByteOrder::Big));
        assert_eq!(archive.payload.as_ref(), parts.concat());
        assert_eq!(frame.data.as_ref(), &[0x34, 0x12, 0xff, 0xff]);
        assert_eq!(
            (frame.width, frame.height, frame.pixel_format),
            (2, 1, 0x0110_00b8)
        );
        let range = GvspPartFormat::Coord3dC16.project(&frame).unwrap();
        let confidence = GvspPartFormat::Confidence8.project(&frame).unwrap();
        assert_eq!(range.data.as_ptr(), frame.data.as_ptr());
        assert_eq!(confidence.data.as_ref(), &[0x80, 0xff]);
        assert_eq!(
            confidence.multipart.as_ref().unwrap().payload.as_ptr(),
            archive.payload.as_ptr()
        );
        assert!(crate::auto::meter(&range).is_none());
        assert!(crate::auto::meter(&confidence).is_none());

        let mut bad_trailer = packets[4].clone();
        bad_trailer[32..36].fill(0);
        let mut assembler = Assembler::new(9, 576, 0);
        assembler.big_endian = true;
        for packet in &packets[..4] {
            assert!(assembler.push(packet).unwrap().is_none());
        }
        let error = assembler.push(&bad_trailer).unwrap_err();
        assert!(error.to_string().contains("trailer height"));
        assert!(assembler.ready.is_empty());
    }

    #[test]
    fn multipart_wire_measurement_representative_does_not_resolve_a_duplicate() {
        use crate::transport::multipart::GvspPartFormat;
        let parts = [vec![0x34, 0x12], vec![0x78, 0x56]];
        let packets = multipart_wire_packets(
            48,
            520,
            &[(5, 0x0110_00b8, 1, 1), (5, 0x0110_00b8, 1, 1)],
            &parts,
            &[0, 1],
        );
        let mut assembler = Assembler::new(4, 576, 0);
        let frame = packets
            .iter()
            .find_map(|packet| assembler.push(packet).unwrap())
            .unwrap();
        assert_eq!(
            frame.multipart.as_ref().unwrap().payload.as_ref(),
            parts.concat()
        );
        assert_eq!(frame.data.as_ref(), parts[0]);
        let error = GvspPartFormat::Coord3dC16.project(&frame).unwrap_err();
        assert_eq!(
            crate::types::error_transport_code(&error),
            Some("unsupported")
        );
        assert!(error.to_string().contains("multiple Coord3D_C16 parts"));
    }

    #[test]
    fn multipart_wire_selects_scalar_parts_with_exact_sample_order() {
        use crate::transport::multipart::GvspPartFormat;
        for (wide_confidence, big_endian, confidence, expected_confidence) in [
            (false, true, GvspPartFormat::Confidence8, [0, 5, 10, 15]),
            (true, true, GvspPartFormat::Confidence16, [5, 0, 15, 10]),
            (true, false, GvspPartFormat::Confidence16, [0, 5, 10, 15]),
        ] {
            let (packets, parts) = multipart_packets_with_confidence(44, 520, wide_confidence);
            let mut assembler = Assembler::new(3000, 576, 0);
            assembler.big_endian = big_endian;
            let frame = packets
                .iter()
                .find_map(|packet| assembler.push(packet).unwrap())
                .unwrap();
            assert_eq!(frame.data.as_ref(), parts[1]);
            let archive = frame.multipart.as_ref().unwrap();
            let range = GvspPartFormat::Coord3dC16.project(&frame).unwrap();
            let confidence = confidence.project(&frame).unwrap();
            assert_eq!(
                (range.width, range.height, range.pixel_format),
                (17, 19, 0x0110_00b8)
            );
            assert_eq!(
                &range.data[..4],
                if big_endian {
                    &[3, 0, 9, 6]
                } else {
                    &[0, 3, 6, 9]
                }
            );
            assert_eq!(&confidence.data[..4], &expected_confidence);
            assert_eq!((confidence.width, confidence.height), (17, 19));
            let inspection = crate::inspect::inspect(
                &confidence,
                crate::camera_profile::CameraKind::AreaScan,
                None,
                crate::inspect::default_edge(),
                crate::inspect::PreviewFormat::Png,
            )
            .unwrap();
            assert!(inspection.measurements.exposure.is_none());
            assert!(inspection.measurements.focus.is_none());
            assert!(inspection.measurements.notes[0].contains("contains measurement samples"));
            for (selected, index) in [(range, 0), (confidence, 2)] {
                let retained = selected.multipart.as_ref().unwrap();
                assert_eq!(retained.layout.selected, index);
                assert_eq!(
                    retained.layout.data_type_namespace,
                    Some(crate::transport::multipart::PartTypeNamespace::Gvsp)
                );
                assert_eq!(retained.payload.as_ptr(), archive.payload.as_ptr());
                assert_eq!(retained.payload.as_ref(), parts.concat());
                retained.layout.validate(retained.payload.len()).unwrap();
            }
        }
    }

    #[test]
    fn multipart_wire_rejects_stale_ambiguous_and_inexact_scalar_selection() {
        use crate::transport::multipart::GvspPartFormat;
        for (mode, expected) in [
            (0, "multiple Coord3D_C16 parts"),
            (1, "required wire type"),
            (2, "unpadded samples"),
            (3, "exact geometry"),
            (4, "sample byte order is unavailable"),
        ] {
            let (mut packets, parts) = multipart_packets(45, 520);
            let mut assembler = Assembler::new(2400, 576, 0);
            // The original packet archive is valid in each case. Only the
            // selected map's identity or sample interpretation is invalid.
            match mode {
                0 => {
                    packets[0][176..178].copy_from_slice(&5u16.to_be_bytes());
                    packets[0][184..188].copy_from_slice(&0x0110_00b8u32.to_be_bytes());
                    packets[7][72..74].copy_from_slice(&5u16.to_be_bytes());
                }
                1 => {
                    packets[0][32..34].copy_from_slice(&1u16.to_be_bytes());
                    packets[7][24..26].copy_from_slice(&1u16.to_be_bytes());
                }
                2 => packets[0][72..74].copy_from_slice(&1u16.to_be_bytes()),
                3 => packets[0][56..60].copy_from_slice(&16u32.to_be_bytes()),
                4 => assembler.big_endian_mono16 = true,
                _ => unreachable!(),
            }
            let frame = packets
                .iter()
                .find_map(|packet| assembler.push(packet).unwrap())
                .unwrap();
            let error = GvspPartFormat::Coord3dC16.project(&frame).unwrap_err();
            assert!(
                error.to_string().contains(expected),
                "mode {mode}: {error:#}"
            );
            assert_eq!(frame.data.as_ref(), parts[1]);
            assert_eq!(frame.multipart.unwrap().payload.as_ref(), parts.concat());
        }
    }

    // Independently authored variable-height wire fixture. Addresses stay at
    // leader maxima; the trailer reports only each actually sent prefix.
    fn shortened_multipart_packets(
        block: u64,
        rows: usize,
        big: bool,
    ) -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
        let words = [
            0x1234u16, 0xffff, 0, 0x8001, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16,
        ];
        let range: Vec<_> = words
            .into_iter()
            .flat_map(|word| {
                if big {
                    word.to_be_bytes()
                } else {
                    word.to_le_bytes()
                }
            })
            .collect();
        let maxima = [
            range,
            (1u8..=20).collect(),
            vec![0xa1, 0xb2, 0xc3, 0xd4, 0xe5],
        ];
        let parts = vec![
            maxima[0][..rows * 8].to_vec(),
            maxima[1][..rows * 4].to_vec(),
            maxima[2].clone(),
        ];
        let descriptors = [
            (5, 0x0110_00b8, 4, 5),
            (1, MONO8, 4, 5),
            (0x8001, 0x8108_0123, 0, 0),
        ];
        let full = multipart_wire_packets(block, 8, &descriptors, &maxima, &[1, 0, 2]);
        let mut packets = vec![full[0].clone()];
        for part in [1usize, 0, 2] {
            let base: usize = maxima[..part].iter().map(Vec::len).sum();
            for (index, bytes) in parts[part].chunks(8).enumerate() {
                let mut data = vec![
                    u8::try_from(part).unwrap(),
                    if (index + 1) * 8 >= parts[part].len() {
                        0x41
                    } else {
                        0
                    },
                ];
                let address = u64::try_from(base + index * 8).unwrap().to_be_bytes();
                data.extend_from_slice(&address[2..]);
                data.extend_from_slice(bytes);
                packets.push(gvsp(
                    block,
                    u32::try_from(packets.len()).unwrap(),
                    7,
                    true,
                    &data,
                ));
            }
        }
        let mut tail = full.last().unwrap()[20..].to_vec();
        for part in 0..3 {
            let length = u64::try_from(parts[part].len()).unwrap().to_be_bytes();
            tail[4 + part * 16 + 2..4 + part * 16 + 8].copy_from_slice(&length[2..]);
            if part != 2 {
                tail[4 + part * 16 + 8..4 + part * 16 + 12]
                    .copy_from_slice(&u32::try_from(rows).unwrap().to_be_bytes());
            }
        }
        packets.push(gvsp(
            block,
            u32::try_from(packets.len()).unwrap(),
            2,
            true,
            &tail,
        ));
        (packets, parts)
    }

    #[test]
    fn shortened_multipart_prefixes_keep_wire_addresses_exact_pixels_and_maximum_allocation() {
        use crate::transport::multipart::GvspPartFormat;
        for big in [false, true] {
            for rows in [2usize, 5] {
                let (packets, parts) = shortened_multipart_packets(61, rows, big);
                for reorder in [false, true] {
                    let mut assembler = Assembler::new(65, 64, 1_000_000);
                    assembler.big_endian = big;
                    let order = if reorder {
                        let mut order: Vec<_> = (0..packets.len()).rev().collect();
                        order.insert(2, 1);
                        order
                    } else {
                        (0..packets.len()).collect()
                    };
                    let mut delivered = None;
                    for index in order {
                        if let Some(frame) = assembler.push(&packets[index]).unwrap() {
                            assert!(delivered.replace(frame).is_none());
                        }
                    }
                    let frame = delivered.unwrap();
                    assert_eq!(
                        (frame.width, frame.height, frame.pixel_format),
                        (4, u32::try_from(rows).unwrap(), MONO8)
                    );
                    assert_eq!(frame.data.as_ref(), parts[1]);
                    let archive = frame.multipart.as_ref().unwrap();
                    assert_eq!(archive.payload.as_ref(), parts.concat());
                    assert_eq!(archive.payload.len(), rows * 12 + 5);
                    assert!(archive.payload.allocated_bytes() >= 65);
                    assert!(
                        frame.buffer_bytes()
                            >= archive.payload.allocated_bytes() + frame.data.allocated_bytes()
                    );
                    let layout = serde_json::to_value(&archive.layout).unwrap();
                    assert_eq!(layout["original_filled_bytes"], rows * 12 + 5);
                    assert_eq!(layout["parts"][0]["height"], 5);
                    assert_eq!(layout["parts"][0]["delivered_height"], rows);
                    assert_eq!(layout["parts"][1]["gvsp"]["address_offset"], 40);
                    assert_eq!(layout["parts"][1]["stored_offset"], rows * 8);
                    assert_eq!(layout["parts"][2]["gvsp"]["address_offset"], 60);
                    assert_eq!(layout["parts"][2]["stored_offset"], rows * 12);
                    if rows == 2 {
                        assert_eq!(layout["parts"][0]["gvsp"]["declared_length"], 40);
                        assert_eq!(layout["parts"][1]["gvsp"]["declared_length"], 20);
                    } else {
                        assert!(layout["parts"][0]["gvsp"].get("declared_length").is_none());
                        assert!(layout["parts"][1]["gvsp"].get("declared_length").is_none());
                    }
                    assert!(layout["parts"][2]["gvsp"].get("declared_length").is_none());
                    let range = GvspPartFormat::Coord3dC16.project(&frame).unwrap();
                    assert_eq!(
                        (range.width, range.height),
                        (4, u32::try_from(rows).unwrap())
                    );
                    let expected: Vec<_> = if big {
                        parts[0]
                            .as_chunks::<2>()
                            .0
                            .iter()
                            .flat_map(|word| [word[1], word[0]])
                            .collect()
                    } else {
                        parts[0].clone()
                    };
                    assert_eq!(range.data.as_ref(), expected);
                    assert_eq!(
                        range.multipart.as_ref().unwrap().payload.as_ptr(),
                        archive.payload.as_ptr()
                    );
                    assert!(crate::auto::meter(&range).is_none());
                    GvspPartFormat::Confidence8.project(&frame).unwrap_err();
                    assert_eq!(archive.payload.as_ref(), parts.concat());
                }
            }
        }
    }

    #[test]
    fn shortened_multipart_resend_requires_every_packet_and_trailer_before_publication() {
        let (packets, parts) = shortened_multipart_packets(62, 2, false);
        let start = Instant::now();
        let mut assembler = Assembler::new(65, 64, 0);
        assembler.resend = true;
        for index in [5, 4, 2, 0, 1] {
            assert!(assembler.push_at(&packets[index], start).unwrap().is_none());
        }
        assembler.poll(start + RESEND_IDLE);
        assert!(assembler.ready.is_empty());
        let recovered_before_resend = assembler.stats.resend_recovered;
        assert!(
            assembler
                .requests
                .iter()
                .any(|&(block, extended, first, last)| block == 62
                    && extended
                    && first <= 3
                    && last >= 3)
        );
        let mut resent = packets[3].clone();
        resent[..2].copy_from_slice(&0x0100u16.to_be_bytes());
        let frame = assembler
            .push_at(&resent, start + RESEND_IDLE)
            .unwrap()
            .unwrap();
        assert_eq!(frame.multipart.unwrap().payload.as_ref(), parts.concat());
        assert_eq!(
            assembler.stats.resend_recovered,
            recovered_before_resend + 1
        );
        assert_eq!(assembler.stats.incomplete_frames, 0);

        let mut no_trailer = Assembler::new(65, 64, 0);
        no_trailer.resend = true;
        for packet in &packets[..packets.len() - 1] {
            assert!(no_trailer.push_at(packet, start).unwrap().is_none());
        }
        no_trailer.poll(start + RESEND_IDLE);
        assert!(no_trailer.ready.is_empty());
        assert!(no_trailer.frames[0].packets.is_none());
    }

    #[test]
    fn shortened_multipart_byte_aligned_packed_rows_remain_exact() {
        use crate::{frame::COORD3D_C12P, transport::multipart::GvspPartFormat};
        let row = [0xbc, 0x3a, 0x12, 0x01, 0xf0, 0xff];
        let parts = [row.repeat(2), vec![1, 2, 3, 4, 5, 6, 7, 8]];
        let descriptors = [(5, COORD3D_C12P, 4, 2), (1, MONO8, 4, 2)];
        let mut packets = multipart_wire_packets(65, 8, &descriptors, &parts, &[1, 0]);
        // Replace only the leader maxima; payload addresses stay in its
        // maximum-size block, while the trailer retains the actual two rows.
        packets[0][34..40].copy_from_slice(&30u64.to_be_bytes()[2..]);
        packets[0][60..64].copy_from_slice(&5u32.to_be_bytes());
        packets[0][82..88].copy_from_slice(&20u64.to_be_bytes()[2..]);
        packets[0][108..112].copy_from_slice(&5u32.to_be_bytes());
        packets[1][22..28].copy_from_slice(&30u64.to_be_bytes()[2..]);
        let mut assembler = Assembler::new(50, 64, 0);
        let frame = packets
            .iter()
            .find_map(|packet| assembler.push(packet).unwrap())
            .unwrap();
        let selected = GvspPartFormat::Coord3dC12p.project(&frame).unwrap();
        assert_eq!(
            (selected.width, selected.height, selected.pixel_format),
            (4, 2, COORD3D_C12P)
        );
        assert_eq!(selected.data.as_ref(), parts[0]);
        let archive = selected.multipart.unwrap();
        assert_eq!(archive.payload.as_ref(), parts.concat());
        assert_eq!(archive.layout.parts[1].gvsp.unwrap().address_offset, 30);
        assert_eq!(archive.layout.parts[1].stored_offset, 12);
    }

    #[test]
    fn shortened_multipart_complete_prefixes_still_require_all_ids_and_one_trailer() {
        let (mut packets, _) = shortened_multipart_packets(66, 2, false);
        // Every actual byte arrives, but ID3 never does. The receiver must
        // retain the gap instead of materializing a complete-looking archive.
        for packet in &mut packets[3..] {
            let id = be32(packet, 16) + 1;
            packet[16..20].copy_from_slice(&id.to_be_bytes());
        }
        let start = Instant::now();
        let mut assembler = Assembler::new(65, 64, 0);
        assembler.resend = true;
        for packet in &packets {
            assert!(assembler.push_at(packet, start).unwrap().is_none());
        }
        assert!(
            assembler.frames[0]
                .multipart
                .as_ref()
                .unwrap()
                .coverage_complete()
        );
        assert!(
            !assembler.frames[0]
                .multipart
                .as_ref()
                .unwrap()
                .is_prepared()
        );
        assembler.poll(start + RESEND_IDLE);
        assert!(assembler.ready.is_empty());
        assert!(
            assembler
                .requests
                .iter()
                .any(|&(block, extended, first, last)| block == 66
                    && extended
                    && first <= 3
                    && last >= 3)
        );

        let (packets, _) = shortened_multipart_packets(67, 2, false);
        let mut assembler = Assembler::new(65, 64, 0);
        assert!(assembler.push(&packets[0]).unwrap().is_none());
        assert!(assembler.push(&packets[5]).unwrap().is_none());
        let mut duplicate = packets[5].clone();
        duplicate[36] ^= 1;
        let error = assembler.push(&duplicate).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("conflicting duplicate GVSP multipart trailer")
        );
        assert!(assembler.ready.is_empty());
        assert!(assembler.frames.is_empty());
    }

    #[test]
    fn shortened_multipart_rejects_unreceived_holes_received_tails_and_bad_extents() {
        for (mode, message) in [
            (0, "coverage hole"),
            (1, "overlapping"),
            (2, "outside a delivered prefix"),
            (3, "outside a delivered prefix"),
            (4, "trailer disagrees"),
            (5, "trailer disagrees"),
            (6, "exact geometry"),
            (7, "trailer height"),
            (8, "trailer disagrees"),
            (9, "part count"),
            (10, "opaque parts"),
            (11, "unpadded"),
            (12, "supported unpadded row format"),
            (13, "exact geometry"),
            (14, "block address range"),
            (15, "conflicting duplicate"),
            (16, "byte-aligned rows"),
        ] {
            let (mut packets, _) = shortened_multipart_packets(63, 2, false);
            let mut order: Vec<_> = (0..packets.len()).collect();
            match mode {
                0 => {
                    packets[2].pop();
                }
                1 => packets[3][22..28].copy_from_slice(&7u64.to_be_bytes()[2..]),
                2 | 3 => {
                    packets[3][22..28].copy_from_slice(&32u64.to_be_bytes()[2..]);
                    if mode == 3 {
                        order = vec![0, 5, 1, 2, 3, 4];
                    }
                }
                4 => packets[5][26..32].copy_from_slice(&41u64.to_be_bytes()[2..]),
                5 => packets[5][32..36].copy_from_slice(&6u32.to_be_bytes()),
                6 => packets[5][26..32].copy_from_slice(&15u64.to_be_bytes()[2..]),
                7 => packets[5][32..36].fill(0),
                8 => packets[5][24..26].copy_from_slice(&6u16.to_be_bytes()),
                9 => {
                    let length = packets[5].len() - 16;
                    packets[5].truncate(length);
                }
                10 => packets[5][58..64].copy_from_slice(&4u64.to_be_bytes()[2..]),
                11 => packets[0][72..74].copy_from_slice(&1u16.to_be_bytes()),
                12 => packets[0][40..44].copy_from_slice(&0x8110_00b8u32.to_be_bytes()),
                13 => packets[5][26..32].copy_from_slice(&40u64.to_be_bytes()[2..]),
                14 => packets[1][22..28].copy_from_slice(&16u64.to_be_bytes()[2..]),
                15 => {
                    let mut duplicate = packets[2].clone();
                    duplicate[28] ^= 1;
                    packets.insert(3, duplicate);
                    order = (0..packets.len()).collect();
                }
                16 => {
                    packets[0][40..44].copy_from_slice(&crate::frame::COORD3D_C12P.to_be_bytes());
                    packets[0][56..60].copy_from_slice(&3u32.to_be_bytes());
                }
                _ => unreachable!(),
            }
            let mut assembler = Assembler::new(65, 64, 0);
            let error = order
                .into_iter()
                .find_map(|index| match assembler.push(&packets[index]) {
                    Ok(frame) => {
                        assert!(frame.is_none(), "mode {mode} published");
                        None
                    }
                    Err(error) => Some(error),
                })
                .unwrap();
            assert!(
                error.to_string().contains(message),
                "mode {mode}: {error:#}"
            );
            assert_eq!(
                crate::types::classify(&error).kind,
                if matches!(mode, 2 | 3 | 10..=12 | 16) {
                    crate::types::ErrorKind::Unsupported
                } else {
                    crate::types::ErrorKind::OperationFailed
                }
            );
            assert!(assembler.ready.is_empty());
            assert!(assembler.frames.is_empty());
            let recovered = frame_packets(64, true, 2, 2, 16)
                .iter()
                .find_map(|packet| assembler.push(packet).unwrap())
                .unwrap();
            assert_eq!(recovered.id, 64);
            assert!(recovered.multipart.is_none());
        }
    }

    #[test]
    fn multipart_reordered_leader_trailer_and_short_part_tails_keep_every_sample() {
        let (packets, parts) = multipart_packets(41, 520);
        let start = Instant::now();
        let mut assembler = Assembler::new(2400, 576, 1_000_000);
        assembler.resend = true;
        // Before the leader: a short tail and its exact duplicate, then the
        // trailer. The header must still be acquired, and only ID 3 is missing.
        for index in [2, 2, 7, 6, 0, 4, 5, 1] {
            assert!(assembler.push_at(&packets[index], start).unwrap().is_none());
            assert!(assembler.predict().is_none());
        }
        let recovered = assembler.stats.resend_recovered;
        let mut resent = packets[3].clone();
        resent[..2].copy_from_slice(&0x0100u16.to_be_bytes());
        let frame = assembler.push_at(&resent, start).unwrap().unwrap();
        assert_eq!(frame.id, 41);
        assert_eq!(frame.device_ts_ns, Some(9_876_000));
        assert_multipart_capture(&frame, &parts);
        assert!(
            assembler
                .requests
                .iter()
                .any(|&(block, extended, first, last)| {
                    block == 41 && extended && first <= 3 && last >= 3
                })
        );
        assert_eq!(assembler.stats.resend_recovered, recovered + 1);
        assert_eq!(assembler.stats.incomplete_frames, 0);
    }

    #[test]
    fn multipart_wire_rejects_holes_overlaps_and_contradictory_metadata() {
        for (mode, message) in [
            (0, "coverage hole"),
            (1, "overlapping"),
            (2, "block address range"),
            (3, "trailer disagrees"),
            (4, "unknown part"),
            (5, "part count"),
            (6, "conflicting duplicate"),
            (7, "mixes multipart and ordinary"),
            (8, "intensity size disagrees"),
        ] {
            let (mut packets, _) = multipart_packets(42, 520);
            let mut assembler = Assembler::new(2400, 576, 0);
            match mode {
                0 => {
                    packets[2].pop();
                }
                1 => {
                    // Tail begins one byte before the end of ID 1.
                    let address = (646u64 + 519).to_be_bytes();
                    packets[2][22..28].copy_from_slice(&address[2..]);
                }
                2 => {
                    // Part 1's packet address cannot restart at the part's
                    // local zero. That address belongs to part 0.
                    packets[1][22..28].fill(0);
                }
                3 => packets[7][25] ^= 1,
                4 => packets[1][20] = 7,
                5 => packets[0][7] = 5,
                6 => {
                    let mut duplicate = packets[1].clone();
                    duplicate[28] ^= 1;
                    packets.insert(2, duplicate);
                }
                7 => packets[1][4] = 0x83,
                8 => packets[0][107] ^= 1,
                _ => unreachable!(),
            }
            let error = packets
                .iter()
                .find_map(|packet| match assembler.push(packet) {
                    Ok(frame) => {
                        assert!(frame.is_none(), "mode {mode} published a partial archive");
                        None
                    }
                    Err(error) => Some(error),
                })
                .unwrap();
            assert!(
                error.to_string().contains(message),
                "mode {mode}: {error:#}"
            );
            assert!(assembler.ready.is_empty());
            assert!(assembler.frames.is_empty());
            let recovered = frame_packets(43, true, 2, 2, 528)
                .iter()
                .find_map(|packet| assembler.push(packet).unwrap())
                .expect("ordinary image recovers on the next block");
            assert_eq!(recovered.id, 43);
            assert!(recovered.multipart.is_none());
        }
    }

    #[test]
    fn multipart_wire_refuses_ambiguous_or_unsupported_layouts() {
        for (mode, message) in [
            (0, "exactly one ordinary intensity"),
            (1, "single top-down zone"),
            (5, "single top-down zone"),
            (2, "chunks"),
            (3, "extended IDs"),
            (4, "size disagrees with its exact geometry"),
        ] {
            let (mut packets, _) = multipart_packets(44, 520);
            let mut assembler = Assembler::new(2400, 576, 0);
            match mode {
                0 => packets[0][184..188].copy_from_slice(&MONO8.to_be_bytes()),
                1 => packets[0][47] = 1,
                5 => packets[0][48] = 0x80,
                2 => packets[0][22] = 0x40,
                3 => packets[0] = gvsp(44, 0, 1, false, &packets[0][20..]),
                4 => {
                    assembler.push(&packets[0]).unwrap();
                    // First part reports one fewer delivered line.
                    packets[7][35] ^= 1;
                }
                _ => unreachable!(),
            }
            let error = assembler
                .push(&packets[if mode == 4 { 7 } else { 0 }])
                .unwrap_err();
            assert_eq!(
                crate::types::classify(&error).kind,
                if mode == 4 {
                    crate::types::ErrorKind::OperationFailed
                } else {
                    crate::types::ErrorKind::Unsupported
                }
            );
            assert!(
                error.to_string().contains(message),
                "mode {mode}: {error:#}"
            );
            assert!(assembler.ready.is_empty());
        }
    }

    #[test]
    fn multipart_batch_never_accounts_for_truncated_payload() {
        let (packets, parts) = multipart_packets(45, 520);
        let mut assembler = Assembler::new(2400, 576, 0);
        for (index, packet) in packets.iter().enumerate() {
            if index != 3 {
                assert!(assembler.push(packet).unwrap().is_none());
            }
        }
        let mut batch = batch::Batch::new(multipart::MAX_METADATA_DATAGRAM);
        let len = packets[3].len();
        batch.rests[0][..len].copy_from_slice(&packets[3]);
        assert!(
            batch
                .take(
                    &mut assembler,
                    (&[], std::ptr::null()),
                    &[batch::Received {
                        len,
                        truncated: true,
                        from_camera: true
                    }],
                    Site::GvspFallback,
                    Instant::now(),
                )
                .is_none()
        );
        assert_multipart_capture(&assembler.push(&packets[3]).unwrap().unwrap(), &parts);
    }

    #[test]
    fn fake_camera_multipart_resend_uses_extended_block_and_packet_ids() {
        let (fake, mut gige) = streaming(|camera| {
            camera.registers.insert(GVCP_CAPABILITY, 0x4);
            camera.resend_status = 0x0100;
        });
        let block = 0x1_0000_0046;
        let block_size = (fake.register(STREAM_PACKET_SIZE) & 0xffff) as usize - 56;
        let (packets, parts) = multipart_packets(block, block_size);
        let destination = {
            let mut camera = fake.camera.lock().unwrap();
            camera.frames.insert(block, packets.clone());
            camera.destination()
        };
        for (index, packet) in packets.iter().enumerate() {
            if index != 2 {
                fake.stream.send_to(packet, destination).unwrap();
            }
        }
        let frame = gige.next_frame(Duration::from_secs(2)).unwrap();
        assert_eq!(frame.id, block);
        assert_multipart_capture(&frame, &parts);
        let requests = &fake.camera.lock().unwrap().requests;
        assert!(requests.iter().any(|(request, _)| {
            *request == resend_packet(be16(request, 6), block, true, 2, 2)
        }));
        assert_eq!(gige.stats().unwrap().resend_recovered, 1);
    }

    const WIDTH: u32 = 200;
    const HEIGHT: u32 = 40;
    #[derive(Default)]
    struct FakeCamera {
        enforce_owner: bool,
        owner: Option<SocketAddrV4>,
        commands: Vec<(u16, SocketAddr)>,
        release_replacement: Option<SocketAddrV4>,
        drop_claim_ack: bool,
        claim_replacement: Option<SocketAddrV4>,
        jump_on_read: Option<(u32, Arc<std::sync::atomic::AtomicU64>, u64)>,
        jump_on_claim: Option<(Arc<std::sync::atomic::AtomicU64>, u64)>,
        registers: HashMap<u32, u32>,
        unreadable: Vec<u32>,
        path_mtu: u32,
        test_packets: bool,
        stores_flags: bool,
        stale: bool,
        clamp: Option<u32>,
        reject_above: Option<u32>,
        test_packet_budget: Option<usize>,
        test_packet_delay: Duration,
        rejected_packet_sizes: Vec<u32>,
        register_reads: Vec<u32>,
        register_writes: Vec<(u32, u32)>,
        memory_writes: Vec<(u32, usize)>,
        no_write_memory: bool,
        memory_reads: Vec<(u32, usize)>,
        little_endian: bool,
        resend_status: u16,
        sticky_flags: u32,
        frames: HashMap<u64, Vec<Vec<u8>>>,
        requests: Vec<(Vec<u8>, u16)>,
        scps_writes: Vec<u32>,
    }
    impl FakeCamera {
        fn controller_write(&mut self, peer: SocketAddrV4, address: u32, value: u32) -> u16 {
            if !self.enforce_owner {
                return 0;
            }
            if address == CONTROL_PRIVILEGE && value == 2 && self.owner.is_none() {
                self.owner = Some(peer);
                self.registers.insert(PRIMARY_PORT, u32::from(peer.port()));
                self.registers.insert(PRIMARY_IP, u32::from(*peer.ip()));
            }
            if self.owner != Some(peer) {
                return 0x8006;
            }
            if address == CONTROL_PRIVILEGE && value == 0 {
                self.owner = None;
                self.registers.insert(PRIMARY_PORT, 0);
                self.registers.insert(PRIMARY_IP, 0);
            }
            0
        }

        fn register(&self, address: u32) -> u32 {
            self.registers.get(&address).copied().unwrap_or_default()
        }
        fn destination(&self) -> SocketAddrV4 {
            SocketAddrV4::new(
                Ipv4Addr::from(self.register(STREAM_ADDRESS)),
                u16::try_from(self.register(STREAM_PORT)).unwrap(),
            )
        }
        fn write(
            &mut self,
            address: u32,
            value: u32,
            outgoing: &mut Vec<(Vec<u8>, SocketAddrV4, Duration)>,
        ) -> u16 {
            self.register_writes.push((address, value));
            if address != STREAM_PACKET_SIZE {
                self.registers.insert(address, value);
                return 0;
            }
            self.scps_writes.push(value);
            if self
                .reject_above
                .is_some_and(|limit| value & 0xffff > limit)
                || self.rejected_packet_sizes.contains(&(value & 0xffff))
            {
                return 0x8002;
            }
            let size = self
                .clamp
                .map_or(value & 0xffff, |clamp| clamp.min(value & 0xffff));
            let can_test = self.test_packet_budget.is_none_or(|left| left > 0);
            if value & SCPS_FIRE != 0 && self.test_packets && can_test {
                if let Some(left) = &mut self.test_packet_budget {
                    *left -= 1;
                }
                let stale = vec![0; size as usize - 29];
                let fired = vec![0; size as usize - 28];
                for (packet, send) in [
                    (&stale, self.stale),
                    (&fired, size <= self.path_mtu),
                    (&stale, self.stale),
                ] {
                    if send {
                        outgoing.push((packet.clone(), self.destination(), self.test_packet_delay));
                    }
                }
            }
            let flags = value
                & if self.stores_flags {
                    0xffff_0000
                } else {
                    0x7fff_0000
                };
            self.registers
                .insert(address, flags | size | self.sticky_flags);
            0
        }
        fn resend(
            &mut self,
            packet: &[u8],
            port: u16,
            outgoing: &mut Vec<(Vec<u8>, SocketAddrV4, Duration)>,
        ) {
            self.requests.push((packet.to_vec(), port));
            let extended = packet[1] & 0x10 != 0;
            let block = if extended {
                be64(packet, 20)
            } else {
                u64::from(be16(packet, 10))
            };
            let Some(packets) = self.frames.get(&block) else {
                return;
            };
            for id in be32(packet, 12)..=be32(packet, 16) {
                let mut resent = packets[id as usize].clone();
                if self.resend_status >= 0x8000 {
                    resent.truncate(if extended { 20 } else { 8 });
                }
                resent[..2].copy_from_slice(&self.resend_status.to_be_bytes());
                outgoing.push((resent, self.destination(), Duration::ZERO));
            }
        }
    }
    // Each fixture runs a UDP server and heartbeat alongside the test. Keep
    // these real-time exchanges from competing with other fake cameras on
    // small runners, without changing protocol deadlines or skipping tests.
    static FAKE_CAMERA: Mutex<()> = Mutex::new(());

    struct Fake {
        _fixture: std::sync::MutexGuard<'static, ()>,
        address: SocketAddrV4,
        gvcp: UdpSocket,
        stream: UdpSocket,
        camera: Arc<Mutex<FakeCamera>>,
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    }
    impl Fake {
        fn new(setup: impl FnOnce(&mut FakeCamera)) -> Self {
            let fixture = FAKE_CAMERA
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut camera = FakeCamera::default();
            camera.registers.insert(HEARTBEAT_TIMEOUT, 3000);
            setup(&mut camera);
            let camera = Arc::new(Mutex::new(camera));
            let gvcp = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            gvcp.set_read_timeout(Some(Duration::from_millis(5)))
                .unwrap();
            let stream = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let SocketAddr::V4(address) = gvcp.local_addr().unwrap() else {
                unreachable!()
            };
            let (server, source, state, stopped) = (
                gvcp.try_clone().unwrap(),
                stream.try_clone().unwrap(),
                camera.clone(),
                stop.clone(),
            );
            let thread = thread::spawn(move || {
                let mut buffer = [0u8; 2048];
                while !stopped.load(Ordering::Relaxed) {
                    let Ok((length, peer)) = server.recv_from(&mut buffer) else {
                        continue;
                    };
                    let packet = &buffer[..length];
                    let (command, id) = (be16(packet, 2), be16(packet, 6));
                    let mut outgoing = Vec::new();
                    let mut camera = state.lock().unwrap();
                    camera.commands.push((command, peer));
                    let reply = match command {
                        READ_REGISTER => {
                            let addresses: Vec<u32> =
                                packet[8..].chunks(4).map(|a| be32(a, 0)).collect();
                            camera.register_reads.extend(&addresses);
                            if let Some((address, clock, value)) = &camera.jump_on_read
                                && camera.owner.is_some()
                                && addresses.contains(address)
                            {
                                clock.store(*value, Ordering::SeqCst);
                            }
                            if addresses.iter().any(|a| camera.unreadable.contains(a)) {
                                Some(ack(command + 1, id, 0x8003, &[]))
                            } else {
                                let values: Vec<u8> = addresses
                                    .iter()
                                    .flat_map(|a| camera.register(*a).to_be_bytes())
                                    .collect();
                                Some(ack(command + 1, id, 0, &values))
                            }
                        }
                        WRITE_REGISTER => {
                            let (address, value) = (be32(packet, 8), be32(packet, 12));
                            let SocketAddr::V4(peer) = peer else {
                                unreachable!()
                            };
                            let access = camera.controller_write(peer, address, value);
                            let status = if access == 0 {
                                camera.write(address, value, &mut outgoing)
                            } else {
                                access
                            };
                            if address == CONTROL_PRIVILEGE
                                && value == 2
                                && let Some((clock, value)) = &camera.jump_on_claim
                            {
                                clock.store(*value, Ordering::SeqCst);
                            }
                            if address == CONTROL_PRIVILEGE
                                && value == 0
                                && status == 0
                                && let Some(replacement) = camera.release_replacement
                            {
                                camera.owner = Some(replacement);
                                camera.registers.insert(CONTROL_PRIVILEGE, 2);
                                camera
                                    .registers
                                    .insert(PRIMARY_PORT, u32::from(replacement.port()));
                                camera
                                    .registers
                                    .insert(PRIMARY_IP, u32::from(*replacement.ip()));
                                None
                            } else if address == CONTROL_PRIVILEGE
                                && value == 2
                                && camera.drop_claim_ack
                            {
                                if let Some(replacement) = camera.claim_replacement {
                                    // Adversarial hidden timeout/owner change,
                                    // restored to the value read before claim.
                                    let old = camera.register(HEARTBEAT_TIMEOUT);
                                    camera.registers.insert(HEARTBEAT_TIMEOUT, 100);
                                    camera.owner = Some(replacement);
                                    camera
                                        .registers
                                        .insert(PRIMARY_PORT, u32::from(replacement.port()));
                                    camera
                                        .registers
                                        .insert(PRIMARY_IP, u32::from(*replacement.ip()));
                                    camera.registers.insert(HEARTBEAT_TIMEOUT, old);
                                }
                                None
                            } else {
                                let acknowledged = 1u32.to_be_bytes();
                                Some(ack(
                                    command + 1,
                                    id,
                                    status,
                                    if status == 0 { &acknowledged } else { &[] },
                                ))
                            }
                        }
                        READ_MEMORY => {
                            let (address, length) = (be32(packet, 8), be32(packet, 12));
                            camera.memory_reads.push((address, length as usize));
                            let mut data = address.to_be_bytes().to_vec();
                            for word in (address..address + length).step_by(4) {
                                let value = camera.register(word);
                                data.extend(if camera.little_endian {
                                    value.to_le_bytes()
                                } else {
                                    value.to_be_bytes()
                                });
                            }
                            Some(ack(command + 1, id, 0, &data))
                        }
                        WRITE_MEMORY if !camera.no_write_memory => {
                            let address = be32(packet, 8);
                            let data = &packet[12..];
                            camera.memory_writes.push((address, data.len()));
                            let little_endian = camera.little_endian;
                            let status = data.chunks(4).enumerate().find_map(|(word, value)| {
                                let address = address + u32::try_from(word).unwrap() * 4;
                                let value = if little_endian {
                                    u32::from_le_bytes(value.try_into().unwrap())
                                } else {
                                    be32(value, 0)
                                };
                                match camera.write(address, value, &mut outgoing) {
                                    0 => None,
                                    status => Some(status),
                                }
                            });
                            // Real devices answer with reserved(16) + index(16).
                            let [high, low] = u16::try_from(data.len()).unwrap().to_be_bytes();
                            let index = [0, 0, high, low];
                            Some(ack(command + 1, id, status.unwrap_or(0), &index))
                        }
                        PACKET_RESEND => {
                            camera.resend(packet, peer.port(), &mut outgoing);
                            None
                        }
                        _ => Some(ack(command + 1, id, 0x8001, &[])),
                    };
                    drop(camera);
                    // Stream I/O must not hold the camera-state lock needed
                    // by independent control and heartbeat requests.
                    for (packet, destination, delay) in outgoing {
                        if delay.is_zero() {
                            let _ = source.send_to(&packet, destination);
                        } else {
                            let source = source.try_clone().unwrap();
                            thread::spawn(move || {
                                thread::sleep(delay);
                                let _ = source.send_to(&packet, destination);
                            });
                        }
                    }
                    if let Some(reply) = reply {
                        let _ = server.send_to(&reply, peer);
                    }
                }
            });
            Self {
                _fixture: fixture,
                address,
                gvcp,
                stream,
                camera,
                stop,
                thread: Some(thread),
            }
        }
        fn register(&self, address: u32) -> u32 {
            self.camera.lock().unwrap().register(address)
        }
        fn send(&self, block: u64, extended: bool, drops: &[u32]) {
            self.send_with(block, extended, drops, |packet, destination| {
                self.stream.send_to(packet, destination).unwrap();
            });
        }
        fn send_with(
            &self,
            block: u64,
            extended: bool,
            drops: &[u32],
            send: impl FnMut(&[u8], SocketAddrV4),
        ) {
            self.send_sized(block, extended, (WIDTH, HEIGHT), drops, send);
        }
        fn send_sized(
            &self,
            block: u64,
            extended: bool,
            (width, height): (u32, u32),
            drops: &[u32],
            mut send: impl FnMut(&[u8], SocketAddrV4),
        ) {
            let (packets, destination) = {
                let mut camera = self.camera.lock().unwrap();
                let size = (camera.register(STREAM_PACKET_SIZE) & 0xffff) as usize;
                let overhead = if extended { 48 } else { 36 };
                let packets = frame_packets(block, extended, width, height, size - overhead);
                camera.frames.insert(block, packets.clone());
                (packets, camera.destination())
            };
            for (id, packet) in packets.iter().enumerate() {
                if !drops.contains(&(u32::try_from(id).unwrap())) {
                    send(packet, destination);
                }
            }
        }
    }
    impl Drop for Fake {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }
    fn streaming(setup: impl FnOnce(&mut FakeCamera)) -> (Fake, GigE) {
        let fake = Fake::new(|camera| {
            camera.path_mtu = 1500;
            camera.test_packets = true;
            setup(camera);
        });
        let mut gige = connect(fake.address, Duration::from_secs(1)).unwrap();
        gige.packet_size = PacketSize::Verified(1500);
        gige.start((WIDTH * HEIGHT) as usize).unwrap();
        (fake, gige)
    }
    fn negotiate(setup: impl FnOnce(&mut FakeCamera), cap: u32, mtu_known: bool) -> Option<u32> {
        let fake = Fake::new(|camera| {
            camera.test_packets = true;
            setup(camera);
        });
        let mut control = Control::connect(fake.address, Duration::from_secs(2)).unwrap();
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = socket.local_addr().unwrap().port();
        control
            .write_register(STREAM_ADDRESS, u32::from(Ipv4Addr::LOCALHOST))
            .unwrap();
        control
            .write_register(STREAM_PORT, u32::from(port))
            .unwrap();
        let mut buffer = vec![0; 65_536];
        let camera = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let size = ladder(
            |size| probe_packet_size(&mut control, &socket, &mut buffer, camera, size),
            cap,
            mtu_known,
        );
        let writes = fake.camera.lock().unwrap().scps_writes.clone();
        let fired = SCPS_FIRE | SCPS_NO_FRAGMENT;
        assert!(writes.iter().all(|value| value & 0xffff_0000 == fired));
        size
    }
    #[test]
    fn discovery_identity_and_truncated_packets() {
        let mut identity = vec![0u8; 0xf8];
        identity[12..16].copy_from_slice(&0x1234_5678_u32.to_be_bytes());
        identity[0x48..0x4c].copy_from_slice(b"Acme");
        identity[0x68..0x6c].copy_from_slice(b"C123");
        identity[0xd8..0xdc].copy_from_slice(b"5678");
        let camera = discovery_info(&identity, Ipv4Addr::new(10, 1, 2, 3)).unwrap();
        assert_eq!(camera.id, "gige:000012345678");
        assert_eq!(camera.address.as_deref(), Some("10.1.2.3"));
        assert_eq!(camera.vendor, "Acme");
        identity[12..16].fill(0);
        let first = discovery_info(&identity, Ipv4Addr::LOCALHOST).unwrap();
        let second = discovery_info(&identity, Ipv4Addr::new(192, 168, 2, 1)).unwrap();
        assert_ne!(
            first.id, second.id,
            "software cameras with no MAC must stay distinct"
        );
        discovery_info(&identity[..240], Ipv4Addr::LOCALHOST).unwrap_err();
        // Anything on the network can answer discovery: its names cannot
        // carry terminal escapes to whoever lists cameras.
        let vendor = b"Vend\x1b]0;PWNED\x07or";
        let model = b"Model\x1b[31mRED\x1b[0m";
        let serial = b"SN\x1b[2Jx\r\n";
        identity[0x48..0x48 + vendor.len()].copy_from_slice(vendor);
        identity[0x68..0x68 + model.len()].copy_from_slice(model);
        identity[0xd8..0xd8 + serial.len()].copy_from_slice(serial);
        let hostile = discovery_info(&identity, Ipv4Addr::new(10, 1, 2, 3)).unwrap();
        for text in [&hostile.vendor, &hostile.model, &hostile.serial] {
            assert!(!text.chars().any(char::is_control), "{text:?}");
        }
        assert_eq!(hostile.model, "Model\u{fffd}[31mRED\u{fffd}[0m");
        assert_eq!(hostile.serial, "SN\u{fffd}[2Jx");
        assert!(parse_ack(&[0; 7]).is_err());
        assert!(parse_ack(&[0, 0, 0, 3, 0, 1, 0, 1]).is_err());
    }
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    #[test]
    fn interface_pinned_discovery_sockets_listen_on_every_address() {
        let loopback = if_addrs::get_if_addrs()
            .unwrap()
            .into_iter()
            .find(|interface| interface.addr.ip() == IpAddr::V4(Ipv4Addr::LOCALHOST))
            .unwrap();
        let socket = interface_pinned_socket(loopback.index.unwrap(), &loopback.name);
        // Older or restricted Linux kernels refuse interface binding, and
        // discovery then falls back to source-bound sockets.
        if socket.is_none() && cfg!(not(target_os = "macos")) {
            return;
        }
        let socket = socket.unwrap();
        let SocketAddr::V4(local) = socket.local_addr().unwrap() else {
            unreachable!()
        };
        // Bound to 0.0.0.0, so broadcast acknowledgements are delivered too.
        assert!(local.ip().is_unspecified());
        let camera = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        socket
            .send_to(b"discovery", camera.local_addr().unwrap())
            .unwrap();
        let mut buffer = [0; 16];
        let (_, peer) = camera.recv_from(&mut buffer).unwrap();
        camera.send_to(b"ack", peer).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        assert_eq!(socket.recv(&mut buffer).unwrap(), 3);
    }
    #[test]
    fn udp_retries_ignore_stale_id_and_wrong_ack_command() {
        let server = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        server
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let SocketAddr::V4(address) = server.local_addr().unwrap() else {
            unreachable!()
        };
        let worker = thread::spawn(move || {
            let mut buffer = [0u8; 100];
            let (length, peer) = server.recv_from(&mut buffer).unwrap();
            assert_eq!(length, 12);
            let id = be16(&buffer, 6);
            // Drop first command so the client must retry the same request ID.
            let (_, _) = server.recv_from(&mut buffer).unwrap();
            assert_eq!(be16(&buffer, 6), id);
            server
                .send_to(
                    &ack(
                        READ_REGISTER + 1,
                        id.wrapping_add(1),
                        0,
                        &7u32.to_be_bytes(),
                    ),
                    peer,
                )
                .unwrap();
            server
                .send_to(&ack(WRITE_REGISTER + 1, id, 0, &1u32.to_be_bytes()), peer)
                .unwrap();
            server
                .send_to(&ack(READ_REGISTER + 1, id, 0, &42u32.to_be_bytes()), peer)
                .unwrap();
        });
        let mut control = Control::connect(address, Duration::from_millis(300)).unwrap();
        assert_eq!(control.read_register(0x100).unwrap(), 42);
        worker.join().unwrap();
    }
    /// A camera that answers each command once, after `reply`, optionally
    /// preceded by a `PENDING_ACK`; returns how many command packets it saw.
    fn slow_camera(pending: Option<u16>, reply: Duration) -> (SocketAddrV4, JoinHandle<usize>) {
        let server = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let SocketAddr::V4(address) = server.local_addr().unwrap() else {
            unreachable!()
        };
        let worker = thread::spawn(move || {
            let mut buffer = [0u8; 100];
            let (_, peer) = server.recv_from(&mut buffer).unwrap();
            let id = be16(&buffer, 6);
            if let Some(completion) = pending {
                let payload = [[0, 0], completion.to_be_bytes()].concat();
                server
                    .send_to(&ack(PENDING_ACK, id, 0, &payload), peer)
                    .unwrap();
            }
            let started = Instant::now();
            let mut commands = 1;
            server
                .set_read_timeout(Some(Duration::from_millis(5)))
                .unwrap();
            while started.elapsed() < reply {
                commands += usize::from(server.recv(&mut buffer).is_ok());
            }
            let value = 42u32.to_be_bytes();
            server
                .send_to(&ack(READ_REGISTER + 1, id, 0, &value), peer)
                .unwrap();
            commands
        });
        (address, worker)
    }

    #[test]
    fn scoped_gvcp_sends_bound_kernel_wait_and_restore_the_previous_socket_option() {
        let server = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let SocketAddr::V4(address) = server.local_addr().unwrap() else {
            unreachable!()
        };
        let mut control = Control::connect(address, Duration::from_secs(1)).unwrap();
        for previous in [None, Some(Duration::from_secs(2))] {
            control.socket.set_write_timeout(previous).unwrap();
            let previous = control.socket.write_timeout().unwrap();
            control.operation_deadline = Some(Instant::now() + Duration::from_secs(1));
            let error = control
                .with_send_deadline(|| -> Result<()> {
                    let scoped = control
                        .socket
                        .write_timeout()?
                        .context("scoped kernel send timeout")?;
                    ensure!(
                        scoped > Duration::ZERO && scoped <= Duration::from_secs(1),
                        "socket send timeout outside caller cap"
                    );
                    Err(Categorized::error(
                        crate::types::ErrorKind::PermissionDenied,
                        "original send failure",
                    ))
                })
                .unwrap_err();
            assert_eq!(
                crate::types::classify(&error).kind,
                crate::types::ErrorKind::PermissionDenied
            );
            assert_eq!(control.socket.write_timeout().unwrap(), previous);
            control.with_send_deadline(|| Ok(())).unwrap();
            assert_eq!(control.socket.write_timeout().unwrap(), previous);
        }
        let (lease, _) = Lease::testing(500, false);
        control.lease = Some(lease);
        control.operation_deadline = Some(Instant::now() + Duration::from_secs(1));
        control
            .with_send_deadline(|| {
                let timeout = control
                    .socket
                    .write_timeout()?
                    .context("finite lease send cap")?;
                ensure!(
                    timeout <= Duration::from_millis(500),
                    "send wait exceeded finite lease proof"
                );
                Ok(())
            })
            .unwrap();
        control.lease = None;
        control.operation_deadline = Some(Instant::now());
        let mut invoked = false;
        let error = control
            .with_send_deadline(|| {
                invoked = true;
                Ok(())
            })
            .unwrap_err();
        assert_eq!(
            crate::types::classify(&error).kind,
            crate::types::ErrorKind::Timeout
        );
        assert!(!invoked);
        control.operation_deadline = None;
        control.socket.set_write_timeout(None).unwrap();
        control
            .with_send_deadline(|| {
                ensure!(
                    control.socket.write_timeout()?.is_none(),
                    "ordinary send inherited scoped timeout"
                );
                Ok(())
            })
            .unwrap();
    }
    #[test]
    fn named_register_deadline_caps_pending_ack_and_late_control_reply() {
        for pending in [None, Some(1000)] {
            let (address, camera) = slow_camera(pending, Duration::from_millis(600));
            let mut control = Control::connect(address, Duration::from_secs(2)).unwrap();
            control.pending_limit = Some(Duration::from_secs(10));
            control.operation_deadline = Some(Instant::now() + Duration::from_millis(500));
            let error = control.read_register(0x100).unwrap_err();
            assert_eq!(
                crate::types::classify(&error).kind,
                crate::types::ErrorKind::Timeout
            );
            assert_eq!(camera.join().unwrap(), 1, "no resend after caller cutoff");
        }
    }
    #[test]
    fn named_register_multichunk_read_and_write_share_one_cutoff() {
        for write in [false, true] {
            let server = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            server
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let SocketAddr::V4(address) = server.local_addr().unwrap() else {
                unreachable!()
            };
            let deadline = Instant::now() + Duration::from_secs(1);
            let camera = thread::spawn(move || {
                let mut buffer = [0; 2048];
                let mut requests = Vec::new();
                for index in 0..2 {
                    let (length, peer) = server.recv_from(&mut buffer).unwrap();
                    let id = be16(&buffer, 6);
                    let command = be16(&buffer, 2);
                    let address = be32(&buffer, 8);
                    requests.push((command, address));
                    assert_eq!(command, if write { WRITE_MEMORY } else { READ_MEMORY });
                    let payload = if write {
                        assert_eq!(length, 12 + MEMORY_CHUNK);
                        vec![0; 4]
                    } else {
                        let mut bytes = address.to_be_bytes().to_vec();
                        bytes.resize(4 + MEMORY_CHUNK, 0x80);
                        bytes
                    };
                    if index == 1 {
                        thread::sleep(
                            deadline.saturating_duration_since(Instant::now())
                                + Duration::from_millis(2),
                        );
                    }
                    server
                        .send_to(&ack(command + 1, id, 0, &payload), peer)
                        .unwrap();
                }
                server
                    .set_read_timeout(Some(Duration::from_millis(50)))
                    .unwrap();
                let error = server.recv_from(&mut buffer).unwrap_err();
                assert!(
                    is_receive_timeout(&error),
                    "no third chunk/retry after cutoff: {error}"
                );
                requests
            });
            let mut control = Control::connect(address, Duration::from_secs(1)).unwrap();
            control.response = Duration::from_secs(1);
            control.operation_deadline = Some(deadline);
            let error = if write {
                control
                    .write_bytes(0x1000, &[0xff; MEMORY_CHUNK * 3])
                    .unwrap_err()
            } else {
                control.read_bytes(0x1000, MEMORY_CHUNK * 3).unwrap_err()
            };
            assert_eq!(
                crate::types::classify(&error).kind,
                crate::types::ErrorKind::Timeout
            );
            assert_eq!(
                camera.join().unwrap(),
                vec![
                    (if write { WRITE_MEMORY } else { READ_MEMORY }, 0x1000),
                    (
                        if write { WRITE_MEMORY } else { READ_MEMORY },
                        0x1000 + u32::try_from(MEMORY_CHUNK).unwrap()
                    )
                ]
            );
        }
    }
    #[test]
    fn named_register_mutex_wait_expires_without_leaking_deadline_to_heartbeat() {
        let (fake, mut gige) = streaming(|_| {});
        let control = gige.control.clone();
        let held = control.lock().unwrap();
        let error = gige
            .read_memory_until(0x100, 4, Instant::now() + Duration::from_millis(10))
            .unwrap_err();
        assert_eq!(
            crate::types::classify(&error).kind,
            crate::types::ErrorKind::Timeout
        );
        assert!(held.operation_deadline.is_none());
        drop(held);
        gige.read_memory_until(0x100, 4, Instant::now() + Duration::from_secs(1))
            .unwrap();
        assert!(control.lock().unwrap().operation_deadline.is_none());
        gige.read_memory(0x100, 4).unwrap();
        let before = fake.camera.lock().unwrap().register_writes.len();
        let error = gige
            .write_memory_until(
                u64::from(HEARTBEAT_TIMEOUT),
                &500u32.to_be_bytes(),
                Instant::now() + Duration::from_secs(1),
            )
            .unwrap_err();
        assert_eq!(
            crate::types::classify(&error).kind,
            crate::types::ErrorKind::Unsupported
        );
        assert_eq!(
            fake.camera.lock().unwrap().register_writes.len(),
            before,
            "named I/O retains managed-register gates"
        );
    }
    #[test]
    fn pending_acks_extend_commands_up_to_the_heartbeat_safe_limit() {
        for limited in [false, true] {
            let (address, camera) = slow_camera(Some(600), Duration::from_millis(400));
            let mut control = Control::connect(address, Duration::from_millis(150)).unwrap();
            control.pending_limit = limited.then_some(Duration::from_secs(2));
            let result = control.read_register(0x100);
            if limited {
                assert_eq!(result.unwrap(), 42);
                assert_eq!(camera.join().unwrap(), 1, "no retransmission while pending");
            } else {
                // Probes keep every command within its own deadline.
                assert!(result.unwrap_err().downcast_ref::<GvcpTimeout>().is_some());
                camera.join().unwrap();
            }
        }
        // The extension never exceeds the limit.
        let (address, camera) = slow_camera(Some(5000), Duration::from_millis(900));
        let mut control = Control::connect(address, Duration::from_millis(150)).unwrap();
        control.pending_limit = Some(Duration::from_millis(500));
        let started = Instant::now();
        control.read_register(0x100).unwrap_err();
        assert!(started.elapsed() < Duration::from_millis(800));
        camera.join().unwrap();
    }
    #[test]
    fn commands_wait_for_the_advertised_response_time_before_retrying() {
        let (address, camera) = slow_camera(None, Duration::from_millis(350));
        let mut control = Control::connect(address, Duration::from_millis(150)).unwrap();
        control.response = Duration::from_millis(400);
        assert_eq!(control.read_register(0x100).unwrap(), 42);
        assert_eq!(camera.join().unwrap(), 1);
        // A lost acknowledgement is still retried within the command time.
        let (address, camera) = slow_camera(None, Duration::from_millis(700));
        let mut control = Control::connect(address, Duration::from_millis(150)).unwrap();
        control.response = Duration::from_millis(400);
        assert_eq!(control.read_register(0x100).unwrap(), 42);
        assert_eq!(camera.join().unwrap(), 2);
    }
    #[test]
    fn connect_enables_pending_acks_and_restores_the_configuration() {
        for (capability, configuration) in
            [(DISABLE_CAPABILITY | 0x20, 0x3), (DISABLE_CAPABILITY, 0x1)]
        {
            let fake = Fake::new(|camera| {
                camera.registers.insert(GVCP_CAPABILITY, capability);
                camera.registers.insert(GVCP_CONFIGURATION, 0x1);
                camera.registers.insert(PENDING_TIMEOUT, 2000);
            });
            let gige = connect(fake.address, Duration::from_millis(200)).unwrap();
            assert_eq!(fake.register(GVCP_CONFIGURATION), configuration);
            // Keep the actual timer; command retries/pending waits are
            // clamped separately rather than silently enlarging the lease.
            assert_eq!(fake.register(HEARTBEAT_TIMEOUT), 3000);
            let control = gige.control.lock().unwrap();
            assert_eq!(control.response, Duration::from_secs(2));
            assert_eq!(control.command_time(), Duration::from_millis(6300));
            assert_eq!(control.pending_limit, Some(Duration::from_millis(2000)));
            drop(control);
            drop(gige);
            assert_eq!(fake.register(GVCP_CONFIGURATION), 0x1);
        }
    }
    #[test]
    fn udp_status_and_address_validation() {
        for invalid_address in [false, true] {
            let server = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            let SocketAddr::V4(address) = server.local_addr().unwrap() else {
                unreachable!()
            };
            let worker = thread::spawn(move || {
                let mut buffer = [0u8; 100];
                let (_, peer) = server.recv_from(&mut buffer).unwrap();
                let id = be16(&buffer, 6);
                let payload = [0x104u32.to_be_bytes().as_slice(), &[0; 8]].concat();
                let response = if invalid_address {
                    ack(READ_MEMORY + 1, id, 0, &payload)
                } else {
                    ack(READ_MEMORY + 1, id, 0x8006, &[])
                };
                server.send_to(&response, peer).unwrap();
            });
            let mut control = Control::connect(address, Duration::from_millis(100)).unwrap();
            let error = control.read_aligned(0x100, 8).unwrap_err().to_string();
            assert!(
                error.contains(if invalid_address {
                    "wrong address"
                } else {
                    "access denied"
                }),
                "{error}"
            );
            worker.join().unwrap();
        }
    }
    #[test]
    fn memory_write_chunks_keep_addresses_and_short_final_payloads() {
        let fake = Fake::new(|_| {});
        let mut control = Control::connect(fake.address, Duration::from_secs(1)).unwrap();
        let base = 0x1000_0000;
        let data: Vec<_> = (0..MEMORY_CHUNK * 2 + 8)
            .map(|i| i.to_le_bytes()[0])
            .collect();
        control.write_bytes(base.into(), &data).unwrap();
        let camera = fake.camera.lock().unwrap();
        let chunk = u32::try_from(MEMORY_CHUNK).unwrap();
        assert_eq!(
            camera.memory_writes,
            [
                (base, MEMORY_CHUNK),
                (base + chunk, MEMORY_CHUNK),
                (base + chunk * 2, 8)
            ]
        );
        for (index, word) in data.as_chunks::<4>().0.iter().enumerate() {
            assert_eq!(
                camera.register(base + u32::try_from(index * 4).unwrap()),
                u32::from_be_bytes(*word)
            );
        }
    }

    #[test]
    fn memory_writes_accept_index_acknowledgements_and_fall_back_to_registers() {
        let name = *b"Line 3\0\0\0\0\0\0\0\0\0\0";
        for no_write_memory in [false, true] {
            let fake = Fake::new(|camera| camera.no_write_memory = no_write_memory);
            let mut control = Control::connect(fake.address, Duration::from_secs(1)).unwrap();
            // DeviceUserID on the 16-byte user-defined name, then an 8-byte register.
            control.write_bytes(0xe8, &name).unwrap();
            control
                .write_bytes(0x1000, &[0, 0, 0, 1, 0, 0, 0, 2])
                .unwrap();
            let camera = fake.camera.lock().unwrap();
            assert_eq!(camera.register(0xf4), u32::from_be_bytes(*b"\0\0\0\0"));
            assert_eq!(camera.register(0xe8), u32::from_be_bytes(*b"Line"));
            assert_eq!((camera.register(0x1000), camera.register(0x1004)), (1, 2));
            assert_eq!(
                camera.memory_writes,
                if no_write_memory {
                    vec![]
                } else {
                    vec![(0xe8, 16), (0x1000, 8)]
                }
            );
        }
        // Without the WRITEMEM capability bit, words go out as register writes.
        let fake = Fake::new(|_| {});
        let mut control = Control::connect(fake.address, Duration::from_secs(1)).unwrap();
        control.write_memory = false;
        control.write_bytes(0xe8, &name).unwrap();
        let camera = fake.camera.lock().unwrap();
        assert_eq!(camera.memory_writes, [] as [(u32, usize); 0]);
        assert_eq!(camera.register_writes.len(), 4);
    }
    #[test]
    fn register_access_follows_the_xml_schema_and_device_byte_order() {
        let root = |attributes: &str| {
            format!(
                "<?xml version=\"1.0\"?>\n<RegisterDescription {attributes} \
                 xmlns=\"http://www.genicam.org/GenApi/Version_1_1\" \
                 xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\" \
                 xsi:schemaLocation=\"x\"><Category Name=\"Root\"/></RegisterDescription>"
            )
        };
        for (attributes, legacy) in [
            (
                r#"ModelName="CD49M11" VendorName="TeledyneDALSA" SchemaMajorVersion="1" SchemaMinorVersion="1" SchemaSubMinorVersion="0""#,
                false,
            ),
            (r#"SchemaMajorVersion="1" SchemaMinorVersion="0""#, true),
            (r#"SchemaMajorVersion="2" SchemaMinorVersion="0""#, false),
            (
                r#"VendorName="Sony" ModelName="XCG_CGSeries" SchemaMajorVersion="1" SchemaMinorVersion="1""#,
                true,
            ),
            ("", true),
        ] {
            assert_eq!(legacy_registers(&root(attributes)), legacy, "{attributes}");
        }
        assert!(legacy_registers("not xml"));

        // A little-endian device holding 1 at a device register.
        for (legacy, write_memory) in [(true, true), (false, true), (false, false)] {
            let fake = Fake::new(|camera| {
                camera.little_endian = true;
                camera.no_write_memory = !write_memory;
                camera.registers.insert(0x2000_0400, 1);
            });
            let mut control = Control::connect(fake.address, Duration::from_secs(1)).unwrap();
            control.legacy_registers = legacy;
            control.little_endian = true;
            control.write_memory = write_memory;
            let read = control.read_bytes(0x2000_0400, 4).unwrap();
            // Schema 1.1 data is in camera order, as its XML declares it.
            assert_eq!(read, if legacy { [0, 0, 0, 1] } else { [1, 0, 0, 0] });
            let written = if legacy { [0, 0, 0, 2] } else { [2, 0, 0, 0] };
            control.write_bytes(0x2000_0400, &written).unwrap();
            assert_eq!(fake.register(0x2000_0400), 2, "{legacy} {write_memory}");
            let camera = fake.camera.lock().unwrap();
            assert_eq!(camera.memory_reads.is_empty(), legacy);
            assert_eq!(camera.memory_writes.is_empty(), legacy || !write_memory);
        }
    }
    #[test]
    fn local_url_hex_and_memory_ranges() {
        let url = parse_local_url("Local:Camera.xml;0x1000;400").unwrap();
        assert_eq!((url.address, url.length), (4096, 1024));
        // Real device and GenTL URLs, including the optional SchemaVersion query.
        for (text, name, address, length) in [
            (
                "Local:Basler_Ace_GigE_e7c9b87e_Version_3_3.zip;c0000000;10cca",
                "Basler_Ace_GigE_e7c9b87e_Version_3_3.zip",
                0xc000_0000,
                0x10cca,
            ),
            (
                "Local:Mikrotron_GmbH_MC206xS11_Rev0_00_007.zip;8001000;395C?SchemaVersion=1.1.0",
                "Mikrotron_GmbH_MC206xS11_Rev0_00_007.zip",
                0x800_1000,
                0x395c,
            ),
            (
                "Local:gigepro_1.12.008.00.xml.zip;FFC10000;0000AD5A?SchemaVersion=1.1.0",
                "gigepro_1.12.008.00.xml.zip",
                0xffc1_0000,
                0xad5a,
            ),
            (
                "local:tlguru_system_rev1.xml;F0F00000;3BF?SchemaVersion=1.0.0",
                "tlguru_system_rev1.xml",
                0xf0f0_0000,
                0x3bf,
            ),
            (
                "Local:///C4_2040_GigE_1.0.0.zip;0x8C400904;0x4D30#fragment",
                "C4_2040_GigE_1.0.0.zip",
                0x8c40_0904,
                0x4d30,
            ),
            (
                "Local: guide_gige_test.zip; 10000000; 1a2b",
                "guide_gige_test.zip",
                0x1000_0000,
                0x1a2b,
            ),
        ] {
            let url = parse_local_url(text).unwrap();
            assert_eq!(
                (url.name.as_str(), url.address, url.length),
                (name, address, length),
                "{text}"
            );
        }
        assert!(parse_local_url("http://camera.example/xml").is_err());
        assert!(parse_local_url("Local:a.xml;0;2000000").is_err());
        assert_eq!(memory_range(0x101, 5).unwrap(), (0x100, 8, 1));
        assert_eq!(
            memory_range(u64::from(u32::MAX), 0).unwrap(),
            (u32::MAX, 0, 0)
        );
        memory_range(u64::from(u32::MAX) + 1, 0).unwrap_err();
        memory_range(u64::from(u32::MAX), 2).unwrap_err();
        assert_eq!(
            memory_range(u64::from(u32::MAX), 1).unwrap(),
            (0xffff_fffc, 4, 3)
        );
    }
    #[test]
    fn xml_padding_is_removed_without_changing_the_document() {
        for prefix in ["", "\u{feff}"] {
            for body in ["<XML/>", " <相机/>\n", "<XML>\0</XML>"] {
                for suffix in ["", "\0", "\0\0"] {
                    assert_eq!(
                        decode_xml("Camera.xml", format!("{prefix}{body}{suffix}").into_bytes())
                            .unwrap(),
                        body
                    );
                }
            }
        }
        for invalid in [
            b"\xef\xbb\xbf<XML>\xff</XML>\0".as_slice(),
            b"\xef\xbb\xbf\xef\xbb\xbf<XML/>",
            b"\xef\xbb\xbf\0",
        ] {
            decode_xml("Camera.xml", invalid.to_vec()).unwrap_err();
        }
    }
    #[test]
    fn zip_xml_load_and_ambiguous_entries() {
        use std::io::Write as _;
        use zip::write::SimpleFileOptions;
        for (count, extension) in [(1, "xml"), (1, "XML"), (1, "xMl"), (2, "XML")] {
            let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
            for index in 0..count {
                writer
                    .start_file(
                        format!("相机{index}.{extension}"),
                        SimpleFileOptions::default(),
                    )
                    .unwrap();
                writer
                    .write_all(b"\xef\xbb\xbf<RegisterDescription/>\0\0")
                    .unwrap();
            }
            let data = writer.finish().unwrap().into_inner();
            let result = decode_xml("Camera.zip", data);
            assert_eq!(result.is_ok(), count == 1);
            if let Ok(xml) = result {
                assert_eq!(xml, "<RegisterDescription/>");
            }
        }
    }
    #[test]
    fn http_xml_is_fetched_fresh_and_failed_http_falls_back_to_local() {
        use std::io::{BufRead as _, BufReader, Write as _};
        use std::net::TcpListener;
        fn put(camera: &mut FakeCamera, address: u32, bytes: &[u8]) {
            for (index, chunk) in bytes.chunks(4).enumerate() {
                let mut word = [0; 4];
                word[..chunk.len()].copy_from_slice(chunk);
                camera.registers.insert(
                    address + u32::try_from(index).unwrap() * 4,
                    u32::from_be_bytes(word),
                );
            }
        }
        for fallback in [false, true] {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            listener.set_nonblocking(true).unwrap();
            let url = format!("http://{}/camera.xml", listener.local_addr().unwrap());
            let first = "<RegisterDescription SchemaMajorVersion=\"1\" SchemaMinorVersion=\"1\" MajorVersion=\"1\"/>";
            let second = first.replace("MajorVersion=\"1\"/>", "MajorVersion=\"2\"/>");
            let fake = Fake::new(|camera| {
                put(camera, 0x200, url.as_bytes());
                if fallback {
                    put(
                        camera,
                        0x400,
                        format!("Local:Camera.xml;10000;{:x}", first.len()).as_bytes(),
                    );
                    put(camera, 0x1_0000, first.as_bytes());
                }
            });
            let served = second.clone();
            let worker = thread::spawn(move || {
                for xml in if fallback {
                    vec![None]
                } else {
                    vec![Some(first), Some(served.as_str())]
                } {
                    let deadline = Instant::now() + Duration::from_secs(3);
                    let mut stream = loop {
                        match listener.accept() {
                            Ok((stream, _)) => break stream,
                            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                                assert!(
                                    Instant::now() < deadline,
                                    "GigE XML HTTP request did not arrive"
                                );
                                thread::sleep(Duration::from_millis(2));
                            }
                            Err(error) => panic!("{error}"),
                        }
                    };
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut request = String::new();
                    loop {
                        let mut line = String::new();
                        assert!(reader.read_line(&mut line).unwrap() > 0);
                        request.push_str(&line);
                        if line == "\r\n" {
                            break;
                        }
                    }
                    assert!(request.starts_with("GET /camera.xml HTTP/1.1\r\n"));
                    if let Some(xml) = xml {
                        write!(
                            stream,
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{xml}",
                            xml.len()
                        )
                        .unwrap();
                    } else {
                        stream
                            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                            .unwrap();
                    }
                }
            });
            let mut gige = connect(fake.address, Duration::from_secs(1)).unwrap();
            assert_eq!(gige.xml().unwrap(), first);
            if !fallback {
                assert!(gige.forget_xml());
                assert_eq!(gige.xml().unwrap(), second);
            }
            worker.join().unwrap();
        }
    }
    #[test]
    fn xml_url_failures_preserve_the_first_typed_cause_and_bounded_diagnostics() {
        use std::io::{BufRead as _, BufReader, Write as _};
        use std::net::TcpListener;
        for kind in [
            crate::types::ErrorKind::Unsupported,
            crate::types::ErrorKind::AuthRequired,
            crate::types::ErrorKind::Timeout,
        ] {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            listener.set_nonblocking(true).unwrap();
            let scheme = if kind == crate::types::ErrorKind::Unsupported {
                "https"
            } else {
                "http"
            };
            let url = format!("{scheme}://{}/camera.xml", listener.local_addr().unwrap());
            let fake = Fake::new(|camera| {
                for (index, chunk) in url.as_bytes().chunks(4).enumerate() {
                    let mut word = [0; 4];
                    word[..chunk.len()].copy_from_slice(chunk);
                    camera.registers.insert(
                        0x200 + u32::try_from(index).unwrap() * 4,
                        u32::from_be_bytes(word),
                    );
                }
            });
            let (stop, wait) = std::sync::mpsc::channel();
            let worker = thread::spawn(move || {
                if kind == crate::types::ErrorKind::Unsupported {
                    return;
                }
                let deadline = Instant::now() + Duration::from_secs(3);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == ErrorKind::WouldBlock => {
                            assert!(
                                Instant::now() < deadline,
                                "GigE XML HTTP request did not arrive"
                            );
                            thread::sleep(Duration::from_millis(2));
                        }
                        Err(error) => panic!("{error}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                loop {
                    let mut line = String::new();
                    assert!(reader.read_line(&mut line).unwrap() > 0);
                    if line == "\r\n" {
                        break;
                    }
                }
                if kind == crate::types::ErrorKind::AuthRequired {
                    stream
                        .write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n")
                        .unwrap();
                } else {
                    wait.recv_timeout(Duration::from_secs(3)).unwrap();
                }
            });
            let mut gige = connect(fake.address, Duration::from_millis(80)).unwrap();
            let error = gige.xml().unwrap_err();
            let _ = stop.send(());
            worker.join().unwrap();
            assert_eq!(crate::types::classify(&error).kind, kind, "{error:#}");
            let message = error.to_string();
            assert!(
                message.contains("URL at 0x200") && message.contains("URL at 0x400"),
                "{message}"
            );
        }
        let error = anyhow!("{}", "相机".repeat(100_000));
        let bounded = xml_diagnostic(&error);
        assert!(bounded.len() <= 4096 + '…'.len_utf8());
        assert!(bounded.ends_with('…'));
    }
    #[test]
    fn reordered_frames_duplicates_and_extended_ids() {
        for extended in [false, true] {
            let id = if extended { 0x1_0000_0001 } else { 15 };
            let mut assembler = Assembler::new(16, if extended { 56 } else { 44 }, 1000);
            assert!(
                assembler
                    .push(&gvsp(id, 2, 3, extended, &[9, 10, 11, 12, 13, 14, 15, 16]))
                    .unwrap()
                    .is_none()
            );
            assert!(
                assembler
                    .push(&gvsp(id, 3, 2, extended, &trailer(2)))
                    .unwrap()
                    .is_none()
            );
            assert!(
                assembler
                    .push(&gvsp(id, 0, 1, extended, &leader(8, 2, MONO8)))
                    .unwrap()
                    .is_none()
            );
            let first = gvsp(id, 1, 3, extended, &[1, 2, 3, 4, 5, 6, 7, 8]);
            let frame = assembler.push(&first).unwrap().unwrap();
            assert_eq!(frame.data, (1..=16).collect::<Vec<_>>());
            assert_eq!(frame.device_ts_ns, Some(1_234_000_000));
            assert_eq!(
                frame.device_timestamp,
                Some(crate::types::DeviceTimestamp {
                    ticks: 1234,
                    frequency_hz: Some(1000),
                })
            );
            assert_eq!(frame.id, id);
            assert!(assembler.push(&first).unwrap().is_none());
        }
    }
    #[test]
    fn an_unknown_clock_frequency_preserves_exact_ticks_without_nanoseconds() {
        let mut assembler = Assembler::new(1, 44, 0);
        let mut metadata = leader(1, 1, MONO8);
        metadata[4..12].copy_from_slice(&u64::MAX.to_be_bytes());
        assembler.push(&gvsp(1, 0, 1, false, &metadata)).unwrap();
        assembler.push(&gvsp(1, 1, 3, false, &[7])).unwrap();
        let frame = assembler
            .push(&gvsp(1, 2, 2, false, &trailer(1)))
            .unwrap()
            .unwrap();
        assert_eq!(frame.device_ts_ns, None);
        assert_eq!(
            frame.device_timestamp,
            Some(crate::types::DeviceTimestamp {
                ticks: u64::MAX,
                frequency_hz: None,
            })
        );
    }
    #[test]
    fn rgb_and_row_padding() {
        let mut assembler = Assembler::new(14, 50, 1_000_000_000);
        let mut metadata = leader(2, 2, RGB8);
        metadata[32..34].copy_from_slice(&1u16.to_be_bytes());
        assembler.push(&gvsp(1, 0, 1, false, &metadata)).unwrap();
        assembler
            .push(&gvsp(
                1,
                1,
                3,
                false,
                &[1, 2, 3, 4, 5, 6, 0, 7, 8, 9, 10, 11, 12, 0],
            ))
            .unwrap();
        let frame = assembler
            .push(&gvsp(1, 2, 2, false, &trailer(2)))
            .unwrap()
            .unwrap();
        assert_eq!(frame.data, (1..=12).collect::<Vec<_>>());
    }
    #[test]
    fn missing_packets_never_emit_and_malformed_frames_are_discarded() {
        let mut assembler = Assembler::new(16, 44, 0);
        assembler
            .push(&gvsp(1, 0, 1, false, &leader(8, 2, MONO8)))
            .unwrap();
        assembler.push(&gvsp(1, 2, 3, false, &[1; 8])).unwrap();
        assert!(
            assembler
                .push(&gvsp(1, 3, 2, false, &trailer(2)))
                .unwrap()
                .is_none()
        );
        assembler.push(&gvsp(1, 1, 3, false, &[2; 7])).unwrap_err();
        assert!(assembler.frames.is_empty());
        assembler
            .push(&gvsp(2, u32::MAX, 3, true, &[0; 8]))
            .unwrap_err();
        for length in 0..8 {
            assert!(parse_header(&[0; 8][..length]).is_err());
        }
        let huge = leader(u32::MAX, u32::MAX, RGB8);
        ImageLeader::parse(&huge, MAX_PAYLOAD).unwrap_err();
    }
    #[test]
    fn incomplete_frames_have_bounded_storage() {
        let mut assembler = Assembler::new(16, 44, 0);
        for frame in 0..100 {
            assembler.push(&gvsp(frame, 1, 3, false, &[0; 8])).unwrap();
            assert!(assembler.frames.len() <= MAX_INCOMPLETE_FRAMES);
        }
        assert_eq!(
            assembler.stats.incomplete_frames,
            100 - MAX_INCOMPLETE_FRAMES as u64
        );
    }
    #[test]
    fn command_payload_lengths_cannot_wrap() {
        let mut payload = vec![0; usize::from(u16::MAX)];
        let packet = command_packet(READ_MEMORY, 7, 1, &payload).unwrap();
        assert_eq!(be16(&packet, 4), u16::MAX);
        assert_eq!(&packet[8..], payload);
        payload.push(0);
        command_packet(READ_MEMORY, 7, 1, &payload).unwrap_err();
    }

    #[test]
    fn camera_tick_conversion_saturates_without_wrapping() {
        assert_eq!(ticks_ns(3, 2), 1_500_000_000);
        assert_eq!(ticks_ns(u64::MAX, 1_000_000_000), u64::MAX);
        assert_eq!(ticks_ns(u64::MAX, 1), u64::MAX);
        assert_eq!(ticks_ns(1, 0), 1_000_000_000);
        assert_eq!(ticks_ns(1, u64::MAX), 0);
    }

    #[test]
    fn packet_resend_commands_match_aravis_captures() {
        use crate::types::hex;
        let standard = resend_packet(0xff15, 0xfffd, false, 3, 4);
        assert_eq!(hex(&standard[..8]), "42000040000cff15");
        assert_eq!(hex(&standard[8..]), "0000fffd0000000300000004");
        let extended = resend_packet(0xff15, 0xfffd, true, 3, 4);
        assert_eq!(hex(&extended[..8]), "421000400014ff15");
        assert_eq!(
            hex(&extended[8..]),
            "000000000000000300000004000000000000fffd"
        );
    }
    #[test]
    fn missing_runs_cover_leader_payload_and_trailer() {
        let bits = |ids: &[u32]| {
            let mut bits = vec![0; 2];
            for &id in ids {
                set_bit(&mut bits, id);
            }
            bits
        };
        assert_eq!(
            missing(&bits(&[1, 2, 5, 6]), Some(6)),
            [(0, 0), (3, 4), (7, 7)]
        );
        assert_eq!(
            missing(&bits(&[0, 1, 2, 3, 4, 5, 6, 7]), Some(6)),
            [] as [(u32, u32); 0]
        );
        assert_eq!(missing(&bits(&[2, 4]), None), [(0, 1), (3, 3)]);
    }
    #[test]
    fn resend_budget_refills_at_4096_packets_per_second() {
        let start = Instant::now();
        let mut budget = Budget {
            tokens: RESEND_BUDGET,
            at: start,
        };
        assert!(budget.take(start, 4096));
        assert!(!budget.take(start, 1));
        let later = start + Duration::from_millis(1);
        assert!(budget.take(later, 4));
        assert!(!budget.take(later, 1));
        assert!(budget.take(start + Duration::from_secs(5), 4096));
    }
    #[test]
    fn late_packets_for_closed_frames_do_not_evict_live_frames() {
        let mut assembler = Assembler::new(16, 44, 0);
        for block in 1..=MAX_INCOMPLETE_FRAMES as u64 + 1 {
            assembler
                .push(&gvsp(block, 0, 1, false, &leader(8, 2, MONO8)))
                .unwrap();
            assembler.push(&gvsp(block, 1, 3, false, &[1; 8])).unwrap();
        }
        let live =
            |assembler: &Assembler| assembler.frames.iter().map(|f| f.block).collect::<Vec<_>>();
        assert_eq!(live(&assembler), [2, 3, 4, 5]);
        assert!(
            assembler
                .push(&gvsp(1, 2, 3, false, &[2; 8]))
                .unwrap()
                .is_none()
        );
        assert_eq!(live(&assembler), [2, 3, 4, 5]);
        assert_eq!(assembler.stats.incomplete_frames, 1);
        let frame = assembler
            .push(&gvsp(2, 2, 3, false, &[2; 8]))
            .unwrap()
            .unwrap();
        assert_eq!(frame.id, 2);
    }
    #[test]
    fn error_status_abandons_frames_without_returning_errors() {
        let mut assembler = Assembler::new(16, 44, 0);
        assembler
            .push(&gvsp(1, 0, 1, false, &leader(8, 2, MONO8)))
            .unwrap();
        let mut unavailable = gvsp(1, 2, 3, false, &[]);
        unavailable[..2].copy_from_slice(&0x800cu16.to_be_bytes());
        assert!(assembler.push(&unavailable).unwrap().is_none());
        assert!(assembler.frames.is_empty());
        assert_eq!(assembler.stats.incomplete_frames, 1);
        assert!(
            assembler
                .push(&gvsp(1, 1, 3, false, &[1; 8]))
                .unwrap()
                .is_none()
        );
        assert!(assembler.frames.is_empty());
        let mut unknown = gvsp(2, 1, 3, false, &[1; 8]);
        unknown[..2].copy_from_slice(&0x0001u16.to_be_bytes());
        assembler.push(&unknown).unwrap_err();

        for status in [0x8010u16, 0x8014] {
            let mut assembler = Assembler::new(16, 44, 0);
            let mut later = gvsp(4, 1, 3, false, &[]);
            later[..2].copy_from_slice(&status.to_be_bytes());
            assembler.push(&later).unwrap();
            assert!(assembler.frames.is_empty() && assembler.closed.is_empty());
            for packet in [
                gvsp(4, 0, 1, false, &leader(8, 2, MONO8)),
                gvsp(4, 2, 3, false, &[2; 8]),
                gvsp(4, 3, 2, false, &trailer(2)),
                later,
            ] {
                assert!(assembler.push(&packet).unwrap().is_none());
            }
            let frame = assembler.push(&gvsp(4, 1, 3, false, &[1; 8])).unwrap();
            assert!(frame.is_some(), "{status:04x}");
        }
    }
    #[test]
    fn delivery_never_goes_backwards() {
        let mut assembler = Assembler::new(48, 44, 0);
        let first = frame_packets(1, false, 8, 6, 8);
        for packet in &first[..3] {
            assembler.push(packet).unwrap();
        }
        let second = frame_packets(2, false, 8, 6, 8);
        let delivered: Vec<_> = second
            .iter()
            .filter_map(|packet| assembler.push(packet).unwrap())
            .collect();
        assert_eq!(delivered.len(), 1);
        assert_eq!(assembler.stats.incomplete_frames, 1);
        for packet in &first[3..] {
            assert!(assembler.push(packet).unwrap().is_none());
        }
        assert!(assembler.frames.is_empty());
    }
    #[test]
    fn frames_complete_without_their_trailer() {
        let start = Instant::now();
        let at = |ms| start + Duration::from_millis(ms);
        let mut assembler = Assembler::new(48, 44, 0);
        let first = frame_packets(1, false, 8, 6, 8);
        for packet in &first[..7] {
            assert!(assembler.push_at(packet, at(0)).unwrap().is_none());
        }
        assembler.poll(at(9));
        assert!(assembler.ready.is_empty());
        assembler.poll(at(10));
        assert_eq!(assembler.ready.pop_front().unwrap().data, image(8, 6));
        let second = frame_packets(2, false, 8, 6, 8);
        for packet in &second[..7] {
            assert!(assembler.push_at(packet, at(20)).unwrap().is_none());
        }
        let next = frame_packets(3, false, 8, 6, 8);
        let frame = assembler.push_at(&next[0], at(21)).unwrap().unwrap();
        assert_eq!(frame.id, 2);
        assert!(assembler.push_at(&second[7], at(22)).unwrap().is_none());
        assert_eq!(assembler.stats.incomplete_frames, 0);
    }
    #[test]
    fn resend_follows_triggers_retries_and_budget() {
        let start = Instant::now();
        let at = |ms| start + Duration::from_millis(ms);
        let mut assembler = resending(48);
        let packets = frame_packets(1, false, 8, 6, 8);
        for id in [0, 1, 2, 4, 5, 6] {
            assembler.push_at(&packets[id], at(0)).unwrap();
        }
        assembler.poll(at(9));
        assert_eq!(assembler.requests, [] as [(u64, bool, u32, u32); 0]);
        assembler.poll(at(10));
        assert_eq!(assembler.requests, [(1, false, 3, 3), (1, false, 7, 7)]);
        for retry in [30, 50] {
            assembler.requests.clear();
            assembler.poll(at(retry - 1));
            assert_eq!(assembler.requests, [] as [(u64, bool, u32, u32); 0]);
            assembler.poll(at(retry));
            assert_eq!(assembler.requests.len(), 2);
        }
        assembler.poll(at(70));
        assert!(assembler.frames.is_empty());
        assert_eq!(assembler.stats.resend_requested, 6);
        assert_eq!(assembler.stats.incomplete_frames, 1);

        assembler.requests.clear();
        let packets = frame_packets(2, false, 8, 6, 8);
        for id in [0, 1, 2, 4, 5, 6, 7] {
            assembler.push_at(&packets[id], at(100)).unwrap();
        }
        assert_eq!(assembler.requests, [(2, false, 3, 3)]);
        let frame = assembler.push_at(&packets[3], at(101)).unwrap().unwrap();
        assert_eq!(frame.data, image(8, 6));
        assert_eq!(assembler.stats.resend_recovered, 1);

        assembler.requests.clear();
        let packets = frame_packets(3, false, 8, 6, 8);
        for id in [0, 1, 2, 4, 5, 6] {
            assembler.push_at(&packets[id], at(200)).unwrap();
        }
        assert_eq!(assembler.requests, [] as [(u64, bool, u32, u32); 0]);
        assembler
            .push_at(&frame_packets(4, false, 8, 6, 8)[0], at(201))
            .unwrap();
        assert_eq!(assembler.requests, [(3, false, 3, 3), (3, false, 7, 7)]);

        let mut assembler = resending(48);
        assembler.budget = Budget {
            tokens: 0.0,
            at: at(300),
        };
        let packets = frame_packets(5, false, 8, 6, 8);
        for id in [0, 1, 2, 4, 5, 6, 7] {
            assembler.push_at(&packets[id], at(300)).unwrap();
        }
        assert_eq!(assembler.requests, [] as [(u64, bool, u32, u32); 0]);
        assembler.poll(at(320));
        assert_eq!(assembler.requests, [(5, false, 3, 3)]);
    }
    #[test]
    fn early_leaders_wait_for_their_data() {
        let start = Instant::now();
        let at = |ms| start + Duration::from_millis(ms);
        for resend in [false, true] {
            let mut assembler = Assembler::new(48, 44, 0);
            assembler.resend = resend;
            let packets = frame_packets(1, false, 8, 6, 8);
            assembler.push_at(&packets[0], at(0)).unwrap();
            for ms in [10, 250, 1000] {
                assembler.poll(at(ms));
            }
            assert!(assembler.requests.is_empty() && !assembler.in_flight());
            let frame = packets[1..]
                .iter()
                .find_map(|packet| assembler.push_at(packet, at(1200)).unwrap())
                .unwrap();
            assert_eq!(frame.data, image(8, 6));
            assert_eq!(assembler.stats.incomplete_frames, 0);
            for (block, ms) in [(2, 1300), (3, 2100)] {
                let packets = frame_packets(block, false, 8, 6, 8);
                assembler.push_at(&packets[0], at(ms)).unwrap();
                assembler.poll(at(ms + 300));
            }
            assert_eq!(assembler.frames.len(), 1);
            assert_eq!(assembler.stats.incomplete_frames, 1);
        }
    }
    #[test]
    fn heavy_loss_is_abandoned_without_requests() {
        let spread = |count: u32| (1..=count).map(|id| id * 2).collect::<Vec<_>>();
        for (packets, drops, abandoned) in [
            (200, (1..=26).collect(), true),
            (200, (1..=25).collect(), false),
            (400, spread(33), true),
            (400, spread(32), false),
        ] {
            let mut assembler = resending(8 * packets);
            for (id, packet) in frame_packets(1, false, 8, u32::try_from(packets).unwrap(), 8)
                .iter()
                .enumerate()
            {
                if !drops.contains(&(u32::try_from(id).unwrap())) {
                    assembler.push(packet).unwrap();
                }
            }
            assert_eq!(assembler.frames.is_empty(), abandoned);
            assert_eq!(assembler.requests.is_empty(), abandoned);
        }
    }
    #[test]
    fn lost_frames_follow_block_gaps_across_the_wrap() {
        let lost = |blocks: Vec<u64>, extended| {
            let mut tracker = Blocks::default();
            blocks
                .into_iter()
                .map(|block| {
                    tracker.seen(block, extended);
                    tracker.delivered(block, extended)
                })
                .sum::<u64>()
        };
        assert_eq!(lost((65_530..=65_535).chain(1..=40).collect(), false), 0);
        assert_eq!(lost((65_530..=65_534).chain(1..=16).collect(), false), 0);
        assert_eq!(lost((65_530..=65_534).chain(1..=17).collect(), false), 1);
        let extended = (0..40)
            .filter(|i| ![5, 6, 7].contains(i))
            .map(|i| u64::from(u32::MAX) + i)
            .collect();
        assert_eq!(lost(extended, true), 3);
        // Gaps that cross the 128-frame bitmap still count every absent
        // block once the 16-frame reordering window has passed it.
        for next in [128, 129, 144, 145, 32_768] {
            let blocks = std::iter::once(1).chain(next..=next + 16).collect();
            assert_eq!(lost(blocks, true), next - 2, "next block {next}");
        }
        let mut reordered: Vec<u64> = (1..=40).collect();
        reordered.swap(4, 9);
        assert_eq!(lost(reordered, false), 0);
        assert_eq!(lost((100..=140).chain(1..=40).collect(), false), 0);
        let mut mixed = Blocks::default();
        mixed.seen(u64::MAX, true);
        mixed.seen(1, false);
        assert_eq!(mixed.delivered(1, false), 0);

        let mut assembler = Assembler::new(48, 44, 0);
        for block in (1..=30).filter(|block| *block != 3) {
            for packet in frame_packets(block, false, 8, 6, 8) {
                assembler.push(&packet).unwrap();
            }
        }
        assert_eq!(assembler.stats.lost_frames, 1);
    }
    #[test]
    fn packet_prediction_bounds_even_an_unlimited_request() {
        let mut assembler = Assembler::new(24, 44, 0);
        let packets = frame_packets(1, false, 8, 3, 8);
        assembler.push(&packets[0]).unwrap();
        assembler.push(&packets[1]).unwrap();
        assert_eq!(assembler.predict_run(0).count(), 0);
        let slots: Vec<_> = assembler.predict_run(usize::MAX).collect();
        assert_eq!(slots.iter().map(|slot| slot.id).collect::<Vec<_>>(), [2, 3]);
        assert_eq!(slots[0].offset, 8);
        assert_eq!(slots[1].offset, 16);
        assert!(slots.iter().all(|slot| slot.len == 8));
        assert_eq!(assembler.predict_run(1).count(), 1);
    }

    #[test]
    fn direct_placement_handles_reordering_duplicates_padding_and_late_leaders() {
        let mut assembler = Assembler::new(32, 44, 0);
        let mut metadata = leader(5, 3, MONO8);
        metadata[32..34].copy_from_slice(&1u16.to_be_bytes());
        metadata[34..36].copy_from_slice(&2u16.to_be_bytes());
        let wire: Vec<u8> = (1..=20).collect();
        let payload = |id: usize| {
            gvsp(
                1,
                u32::try_from(id).unwrap(),
                3,
                false,
                &wire[(id - 1) * 8..(id * 8).min(20)],
            )
        };
        for packet in [
            payload(3),
            gvsp(1, 4, 2, false, &trailer(3)),
            payload(1),
            payload(1),
        ] {
            assert!(assembler.push(&packet).unwrap().is_none());
        }
        assert!(
            assembler
                .push(&gvsp(1, 0, 1, false, &metadata))
                .unwrap()
                .is_none()
        );
        let frame = assembler.push(&payload(2)).unwrap().unwrap();
        assert_eq!(
            frame.data,
            [1, 2, 3, 4, 5, 7, 8, 9, 10, 11, 13, 14, 15, 16, 17]
        );

        let mut assembler = Assembler::new(32, 44, 0);
        assembler.push(&gvsp(3, 2, 2, false, &trailer(1))).unwrap();
        assembler.push(&gvsp(3, 2, 3, false, &[1; 8])).unwrap_err();
        assembler.push(&gvsp(1, 1, 3, false, &[1; 8])).unwrap();
        assembler.push(&gvsp(1, 1, 3, false, &[2; 8])).unwrap_err();
        assembler.push(&gvsp(2, 2, 3, false, &[1; 8])).unwrap();
        assembler.push(&gvsp(2, 1, 3, false, &[1; 4])).unwrap_err();
        assembler.push(&gvsp(4, 1, 3, false, &[1; 4])).unwrap();
        assembler.push(&gvsp(4, 2, 3, false, &[1; 8])).unwrap_err();
        assembler.push(&gvsp(5, 2, 3, false, &[1; 4])).unwrap();
        assembler
            .push(&gvsp(5, 0, 1, false, &leader(4, 1, MONO8)))
            .unwrap_err();
        assert!(assembler.frames.is_empty());
    }
    #[test]
    fn padded_last_packets_are_accepted_inside_and_beyond_the_payload_size() {
        // Teledyne DALSA Genie Nano M1450: 728x544 Mono8 at a 1500-byte packet
        // size reports PayloadSize 396,744 = 271 full 1464-byte blocks; the
        // image needs 396,032, so the last packet carries 712 padding bytes.
        let (width, height, block) = (728, 544, 1464);
        let pixels = image(width, height);
        let mut packets = vec![gvsp(1, 0, 1, false, &leader(width, height, MONO8))];
        for chunk in pixels.chunks(block) {
            let mut data = chunk.to_vec();
            data.resize(block, 0);
            packets.push(gvsp(
                1,
                u32::try_from(packets.len()).unwrap(),
                3,
                false,
                &data,
            ));
        }
        assert_eq!(packets.len() - 1, 271);
        packets.push(gvsp(1, 272, 2, false, &trailer(height)));
        for payload in [396_744, 396_032] {
            for late_leader in [false, true] {
                let mut assembler = Assembler::new(payload, 1500, 0);
                let mut order: Vec<_> = packets.iter().collect();
                if late_leader {
                    order.rotate_left(1);
                }
                let delivered: Vec<_> = order
                    .into_iter()
                    .filter_map(|packet| assembler.push(packet).unwrap())
                    .collect();
                assert_eq!(delivered.len(), 1, "{payload} {late_leader}");
                assert_eq!(delivered[0].data, pixels);
            }
            // A resend of the last packet may drop or add the padding.
            let unpadded = gvsp(1, 271, 3, false, &pixels[270 * block..]);
            for sequence in [[&packets[271], &unpadded], [&unpadded, &packets[271]]] {
                let mut assembler = Assembler::new(payload, 1500, 0);
                for packet in packets[..271].iter().chain(sequence) {
                    assert!(assembler.push(packet).unwrap().is_none());
                }
                let frame = assembler.push(&packets[272]).unwrap().unwrap();
                assert_eq!(frame.data, pixels);
            }
        }
        let mut assembler = Assembler::new(396_744, 1500, 0);
        for packet in &packets[..=271] {
            assembler.push(packet).unwrap();
        }
        assembler
            .push(&gvsp(1, 270, 3, false, &pixels[269 * block..][..752]))
            .unwrap_err();
        let mut assembler = Assembler::new(396_744, 1500, 0);
        assembler.push(&packets[0]).unwrap();
        let short = gvsp(1, 271, 3, false, &pixels[270 * block..][..700]);
        assembler.push(&short).unwrap();
        assembler.push(&packets[272]).unwrap_err();
    }
    #[test]
    fn short_frames_end_at_the_trailer_height() {
        // A 4096-pixel Mono8 line-scan camera configured for 1000 lines at a
        // 9000-byte packet size ends a frame after 300 lines (frame active
        // trigger or line timeout): 137 full 8964-byte blocks, a 732-byte
        // last packet and a trailer with ID 139 and size_y 300.
        let (width, block) = (4096, 8964);
        let lines = image(width, 300);
        let mut packets = vec![gvsp(7, 0, 1, false, &leader(width, 1000, MONO8))];
        for chunk in lines.chunks(block) {
            packets.push(gvsp(
                7,
                u32::try_from(packets.len()).unwrap(),
                3,
                false,
                chunk,
            ));
        }
        assert_eq!((packets.len(), packets[138].len()), (139, 8 + 732));
        packets.push(gvsp(7, 139, 2, false, &trailer(300)));
        for order in [
            (0..140).collect::<Vec<_>>(),
            (1..140).chain([0]).collect(),
            [0, 139].into_iter().chain(1..139).collect(),
        ] {
            let mut assembler = Assembler::new(4_096_000, 9000, 0);
            let delivered: Vec<_> = order
                .iter()
                .filter_map(|id| assembler.push(&packets[*id]).unwrap())
                .collect();
            assert_eq!(delivered.len(), 1, "{order:?}");
            assert_eq!(
                (delivered[0].height, &delivered[0].data.to_vec()),
                (300, &lines)
            );
        }
        let start = Instant::now();
        let mut assembler = Assembler::new(4_096_000, 9000, 0);
        for packet in &packets[..139] {
            assembler.push_at(packet, start).unwrap();
        }
        assembler.poll(start + RESEND_IDLE);
        assert!(assembler.ready.is_empty());

        // A cut on a packet boundary has no short packet, and padded rows
        // are compacted for the delivered height only.
        let mut assembler = Assembler::new(48, 44, 0);
        let mut metadata = leader(5, 8, MONO8);
        metadata[32..34].copy_from_slice(&1u16.to_be_bytes());
        let rows: Vec<u8> = (1..=24).collect();
        for (id, chunk) in rows.chunks(8).enumerate() {
            assembler
                .push(&gvsp(1, u32::try_from(id).unwrap() + 1, 3, false, chunk))
                .unwrap();
        }
        assembler.push(&gvsp(1, 0, 1, false, &metadata)).unwrap();
        let frame = assembler
            .push(&gvsp(1, 4, 2, false, &trailer(4)))
            .unwrap()
            .unwrap();
        assert_eq!(frame.height, 4);
        assert_eq!(frame.data.len(), 20);
        assert_eq!(frame.data[5..10], [7, 8, 9, 10, 11]);

        for (height, last) in [(1001, 732), (0, 732), (200, 732), (300, 700)] {
            let mut assembler = Assembler::new(4_096_000, 9000, 0);
            let result = packets[..138]
                .iter()
                .chain([
                    &gvsp(7, 138, 3, false, &lines[137 * block..][..last]),
                    &gvsp(7, 139, 2, false, &trailer(height)),
                ])
                .try_for_each(|packet| assembler.push(packet).map(drop));
            assert!(result.is_err(), "{height} {last}");
        }
    }
    #[test]
    fn frames_that_pause_mid_stream_wait_for_their_lines() {
        // The same line-scan camera pauses after 137 full packets (or after
        // 434, close to the leader's 1000 lines) until its line timeout ends
        // the frame. Resend is on; only a short unsent tail is requested.
        let (width, block) = (4096, 8964);
        let start = Instant::now();
        let at = |ms| start + Duration::from_millis(ms);
        let resending = || {
            let mut assembler = Assembler::new(4_096_000, 9000, 0);
            assembler.resend = true;
            assembler
        };
        for (lines, full, resumed, requests) in [
            (300, 137, 100, Vec::new()),
            (950, 434, 150, vec![(7, false, 435, 458); 3]),
        ] {
            let pixels = image(width, lines);
            let mut packets = vec![gvsp(7, 0, 1, false, &leader(width, 1000, MONO8))];
            for chunk in pixels.chunks(block) {
                packets.push(gvsp(
                    7,
                    u32::try_from(packets.len()).unwrap(),
                    3,
                    false,
                    chunk,
                ));
            }
            assert_eq!(packets.len(), full + 2);
            packets.push(gvsp(
                7,
                u32::try_from(full).unwrap() + 2,
                2,
                false,
                &trailer(lines),
            ));
            let mut assembler = resending();
            for packet in &packets[..=full] {
                assembler.push_at(packet, at(0)).unwrap();
            }
            for ms in (10..=resumed).step_by(10) {
                assembler.poll(at(ms));
            }
            assert_eq!(assembler.requests, requests, "{lines}");
            let frame = packets[full + 1..]
                .iter()
                .find_map(|packet| assembler.push_at(packet, at(resumed)).unwrap())
                .unwrap();
            assert_eq!((frame.height, frame.data.to_vec()), (lines, pixels));
            assert_eq!(assembler.stats.incomplete_frames, 0);
        }
        // A pause longer than FRAME_IDLE still loses the frame.
        let mut assembler = resending();
        assembler
            .push_at(&gvsp(7, 0, 1, false, &leader(width, 1000, MONO8)), at(0))
            .unwrap();
        assembler
            .push_at(&gvsp(7, 1, 3, false, &image(width, 3)[..block]), at(0))
            .unwrap();
        for ms in (10..=250).step_by(10) {
            assembler.poll(at(ms));
        }
        assert!(assembler.frames.is_empty() && assembler.requests.is_empty());
        assert_eq!(assembler.stats.incomplete_frames, 1);
    }
    #[test]
    fn packed_rows_may_be_padded_per_line_or_per_image() {
        // 5 Mono12p or 6 Mono10p pixels are 60 bits: 8 bytes per line when
        // each line is padded, 60 bytes for 8 lines when lines run on.
        let send = |pixel, width, payload, wire: &[u8], x_padding: u16, y_padding: u16| {
            let mut assembler = Assembler::new(payload, 48, 0);
            let mut metadata = leader(width, 8, pixel);
            metadata[32..34].copy_from_slice(&x_padding.to_be_bytes());
            metadata[34..36].copy_from_slice(&y_padding.to_be_bytes());
            let mut packets = vec![gvsp(1, 0, 1, false, &metadata)];
            for chunk in wire.chunks(12) {
                packets.push(gvsp(
                    1,
                    u32::try_from(packets.len()).unwrap(),
                    3,
                    false,
                    chunk,
                ));
            }
            packets.push(gvsp(
                1,
                u32::try_from(packets.len()).unwrap(),
                2,
                false,
                &trailer(8),
            ));
            packets
                .iter()
                .find_map(|packet| assembler.push(packet).transpose())
                .expect("the image or its error is delivered")
        };
        let pack = |samples: &[u16], bits: usize| {
            let mut data = vec![0u8; (samples.len() * bits).div_ceil(8)];
            for (i, &sample) in samples.iter().enumerate() {
                for bit in 0..bits {
                    data[(i * bits + bit) / 8] |=
                        (u8::from((sample >> bit) & 1 != 0)) << ((i * bits + bit) % 8);
                }
            }
            data
        };
        for (pixel, width, bits) in [(0x010c_0047, 5, 12), (0x010a_0046, 6, 10)] {
            let samples: Vec<u16> = (0..width * 8)
                .map(|i| u16::try_from((i * 317 + 179) & ((1 << bits) - 1)).unwrap())
                .collect();
            let continuous = pack(&samples, bits);
            let rows: Vec<u8> = samples
                .chunks_exact(width as usize)
                .flat_map(|row| pack(row, bits))
                .collect();
            assert_eq!((continuous.len(), rows.len()), (60, 64));
            for payload in [60, 64] {
                let frame = send(pixel, width, payload, &continuous, 0, 0).unwrap();
                assert_eq!(frame.data, rows, "{pixel:08x} {payload}");
                let actual = crate::frame::unpacked(&frame).unwrap();
                let expected: Vec<u8> = samples.iter().flat_map(|v| v.to_le_bytes()).collect();
                assert_eq!(actual.data, expected, "{pixel:08x} {payload}");
            }
            let frame = send(pixel, width, 64, &rows, 0, 0).unwrap();
            assert_eq!(frame.data, rows);
            let mut y_padded = continuous.clone();
            y_padded.extend([0xa5; 3]);
            assert_eq!(send(pixel, width, 63, &y_padded, 0, 3).unwrap().data, rows);
            let x_padded: Vec<u8> = rows
                .as_chunks::<8>()
                .0
                .iter()
                .flat_map(|row| row.iter().copied().chain([0xa5]))
                .collect();
            assert_eq!(send(pixel, width, 72, &x_padded, 1, 0).unwrap().data, rows);
            send(pixel, width, 64, &continuous[..59], 0, 0).unwrap_err();
        }
        for (pixel, width, channels, bits) in [
            (0x0104_0039, 3, 1, 4),
            (0x021e_005c, 3, 3, 10),
            (0x0224_0049, 3, 3, 12),
        ] {
            let samples: Vec<u16> = (0..width * channels * 8)
                .map(|i| u16::try_from((i * 317 + 179) & ((1 << bits) - 1)).unwrap())
                .collect();
            let continuous = pack(&samples, bits);
            let rows: Vec<u8> = samples
                .chunks_exact((width * channels) as usize)
                .flat_map(|row| pack(row, bits))
                .collect();
            let frame = send(pixel, width, rows.len(), &continuous, 0, 0).unwrap();
            assert_eq!(frame.data, rows, "{pixel:08x}");
            crate::frame::rgb(&frame).unwrap();
        }
        ImageLeader::parse(&leader(5, 8, 0x010c_0047), 59).unwrap_err();
    }
    #[test]
    fn image_payloads_with_chunks_deliver_the_image() {
        // Extended chunk mode (payload type 0x4001): the image, then chunk
        // data, here a 64-bit timestamp chunk followed by its ID and length.
        // The trailer adds the chunk payload length and chunk layout ID.
        let mut metadata = leader(8, 2, MONO8);
        metadata[2..4].copy_from_slice(&0x4001u16.to_be_bytes());
        let pixels = image(8, 2);
        let mut payload = pixels.clone();
        payload.extend(0x0123_4567_89ab_cdefu64.to_be_bytes());
        payload.extend(0x0a5a_0001u32.to_be_bytes());
        payload.extend(8u32.to_be_bytes());
        let mut tail = vec![0, 0, 0x40, 0x01];
        tail.extend(2u32.to_be_bytes());
        tail.extend(16u32.to_be_bytes());
        tail.extend(1u32.to_be_bytes());
        let mut packets = vec![gvsp(3, 0, 1, false, &metadata)];
        for chunk in payload.chunks(8) {
            packets.push(gvsp(
                3,
                u32::try_from(packets.len()).unwrap(),
                3,
                false,
                chunk,
            ));
        }
        packets.push(gvsp(3, 5, 2, false, &tail));
        for late_leader in [false, true] {
            let mut assembler = Assembler::new(32, 44, 0);
            let mut order: Vec<_> = packets.iter().collect();
            if late_leader {
                order.rotate_left(1);
            }
            let frame = order
                .into_iter()
                .find_map(|packet| assembler.push(packet).unwrap())
                .unwrap();
            assert_eq!((frame.height, frame.data.to_vec()), (2, pixels.clone()));
            let chunks = frame.chunks.unwrap();
            assert_eq!(
                (chunks.layout_id, chunks.byte_order),
                (Some(1), Some(ByteOrder::Big))
            );
            assert_eq!(&*chunks.payload, &payload);
            assert_eq!(chunks.image_bytes, 16);
            assert_eq!(
                chunks.entries,
                vec![super::super::chunks::Chunk {
                    id: 0x0a5a_0001,
                    offset: 16,
                    length: 8
                }]
            );
        }
        let start = Instant::now();
        let mut assembler = Assembler::new(32, 44, 0);
        for packet in &packets[..5] {
            assembler.push_at(packet, start).unwrap();
        }
        assembler.poll(start + RESEND_IDLE);
        assert!(assembler.ready.is_empty());
        assert!(assembler.push(&packets[5]).unwrap().is_some());

        // Chunk lengths and extended trailer fields are validated before
        // any image can be delivered, including a reordered leader.
        for chunk_bytes in [7u32, 17, u32::MAX] {
            let mut wrong_tail = tail.clone();
            wrong_tail[8..12].copy_from_slice(&chunk_bytes.to_be_bytes());
            let mut assembler = Assembler::new(32, 44, 0);
            for packet in &packets[..5] {
                assembler.push(packet).unwrap();
            }
            assembler
                .push(&gvsp(3, 5, 2, false, &wrong_tail))
                .unwrap_err();
            assert!(assembler.ready.is_empty());
        }
        ImageTrailer::parse(5, &tail[..8]).unwrap_err();

        for (payload_type, name) in [
            (0x0004, "chunk data"),
            (0x0006, "JPEG"),
            (0x0008, "H.264"),
            (0x000a, "multi-part"),
            (0x000b, "GenDC"),
            (0x8001, "device-specific"),
        ] {
            metadata[2..4].copy_from_slice(&u16::try_from(payload_type).unwrap().to_be_bytes());
            let error = ImageLeader::parse(&metadata, 32).unwrap_err();
            assert!(error.to_string().contains(name), "{error}");
            assert_eq!(
                crate::types::error_transport_code(&error),
                Some("unsupported")
            );
        }
    }
    #[test]
    fn manta_g125b_captured_legacy_chunk_headers_keep_their_wire_field_order() {
        // Captured headers only; no camera payload is available in this source.
        // Allied Vision Manta G-125B, GV 1.2, firmware 00.01.44.28514.
        // Provenance: viva-genicam cdfedcc9d52694b1f471029bf585b0e50d04073b,
        // crates/viva-gige/src/gvsp.rs, a_gige_vision_1_extended_chunk_block_is_an_image_with_chunks.
        #[rustfmt::skip]
        const LEADER: [u8; 44] = [
            0x00,0x00,0x00,0x01,0x01,0x00,0x00,0x00,
            0x00,0x01,0x00,0x05,0x00,0x00,0x00,0x2e,
            0xf1,0xc1,0x84,0xe8,0x01,0x08,0x00,0x01,
            0x00,0x00,0x05,0x0c,0x00,0x00,0x03,0xc4,
            0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,
            0x00,0x00,0x00,0x00,
        ];
        #[rustfmt::skip]
        const TRAILER: [u8; 24] = [
            0x00,0x00,0x00,0x01,0x02,0x00,0x03,0x54,
            0x00,0x00,0x00,0x05,0x00,0x13,0x01,0x68,
            0x00,0x00,0x03,0xc4,0x00,0x00,0x00,0x01,
        ];
        let head = parse_gvsp(&LEADER, Site::GvspAssemble).unwrap();
        let tail = parse_gvsp(&TRAILER, Site::GvspAssemble).unwrap();
        assert_eq!((head.frame, head.id, head.extended), (1, 0, false));
        assert_eq!((tail.frame, tail.id, tail.extended), (1, 852, false));
        let leader = ImageLeader::parse(head.data.bytes(), 1_245_544).unwrap();
        let trailer = ImageTrailer::parse(tail.id, tail.data.bytes()).unwrap();
        assert_eq!(
            (leader.width, leader.height, leader.pixel_format),
            (1292, 964, MONO8)
        );
        assert_eq!(leader.payload_type, 5);
        assert!(leader.chunks);
        assert_eq!(trailer.height, 964);
        assert_eq!(trailer.chunks, Some((1_245_544, 1)));
        check_trailer(&leader, 1464, trailer).unwrap();
    }

    // An independently constructed small body using the proved legacy envelope:
    // 18 image/padding bytes + image footer + eight-byte chunk + chunk footer.
    fn legacy_chunk_fixture() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let mut metadata = leader(3, 2, MONO16);
        metadata[2..4].copy_from_slice(&5u16.to_be_bytes());
        metadata[32..34].copy_from_slice(&2u16.to_be_bytes());
        metadata[34..36].copy_from_slice(&2u16.to_be_bytes());
        let mut payload = Vec::new();
        for row in [[0x0102u16, 0x0304, 0x0506], [0x0708, 0x090a, 0x0b0c]] {
            payload.extend(row.into_iter().flat_map(u16::to_be_bytes));
            payload.extend([0xfa, 0xfb]);
        }
        payload.extend([0xfc, 0xfd]);
        payload.extend(1u32.to_be_bytes());
        payload.extend(18u32.to_be_bytes());
        payload.extend(u64::MAX.to_be_bytes());
        payload.extend(0x3e8u32.to_be_bytes());
        payload.extend(8u32.to_be_bytes());
        let mut tail = vec![0, 0, 0, 5];
        tail.extend(42u32.to_be_bytes());
        tail.extend(2u32.to_be_bytes());
        tail.extend(7u32.to_be_bytes());
        (metadata, payload, tail)
    }

    #[test]
    fn legacy_chunk_capture_retains_exact_bytes_with_reordering_and_resend() {
        for extended in [false, true] {
            for delivery in 0..3 {
                let (metadata, payload, tail) = legacy_chunk_fixture();
                let mut packets = vec![gvsp(7, 0, 1, extended, &metadata)];
                for (index, bytes) in payload.chunks(8).enumerate() {
                    packets.push(gvsp(
                        7,
                        u32::try_from(index + 1).unwrap(),
                        3,
                        extended,
                        bytes,
                    ));
                }
                packets.push(gvsp(7, 7, 2, extended, &tail));
                let mut assembler = Assembler::new(42, if extended { 56 } else { 44 }, 0);
                assembler.big_endian = true;
                assembler.resend = delivery == 2;
                let start = Instant::now();
                let order = if delivery == 1 {
                    vec![7, 6, 3, 1, 1, 4, 2, 0, 5]
                } else {
                    (0..packets.len())
                        .filter(|index| delivery != 2 || *index != 2)
                        .collect()
                };
                let mut frame = None;
                for index in order {
                    if let Some(complete) = assembler.push_at(&packets[index], start).unwrap() {
                        assert!(frame.replace(complete).is_none());
                    }
                }
                if delivery == 2 {
                    assert!(frame.is_none());
                    assembler.poll(start + RESEND_IDLE);
                    assert!(
                        assembler
                            .requests
                            .iter()
                            .any(|(block, ids, first, last)| *block == 7
                                && *ids == extended
                                && *first <= 2
                                && *last >= 2)
                    );
                    let mut resent = packets[2].clone();
                    resent[..2].copy_from_slice(&0x0100u16.to_be_bytes());
                    frame = assembler.push_at(&resent, start + RESEND_IDLE).unwrap();
                    assert_eq!(assembler.stats.resend_recovered, 1);
                }
                let frame = frame.unwrap();
                assert_eq!(
                    (frame.width, frame.height, frame.pixel_format),
                    (3, 2, MONO16)
                );
                assert_eq!(frame.data.as_ref(), [2, 1, 4, 3, 6, 5, 8, 7, 10, 9, 12, 11]);
                let chunks = frame.chunks.unwrap();
                assert_eq!(chunks.payload.as_ref(), payload);
                assert_eq!((chunks.image_bytes, chunks.payload_bytes), (18, 42));
                assert_eq!(
                    (chunks.layout_id, chunks.byte_order),
                    (Some(7), Some(ByteOrder::Big))
                );
                assert_eq!(
                    chunks
                        .entries
                        .iter()
                        .map(|chunk| (chunk.id, chunk.offset, chunk.length))
                        .collect::<Vec<_>>(),
                    [(1, 0, 18), (0x3e8, 26, 8)]
                );
                let layout = chunks.image_layout.unwrap();
                assert_eq!(
                    (
                        layout.height,
                        layout.row_bytes,
                        layout.row_stride,
                        layout.padding_y
                    ),
                    (2, 6, 8, 2)
                );
                assert_eq!(layout.byte_order, Some(ByteOrder::Big));
                // This XML is synthetic, not a claim about the Manta's legacy
                // ancillary field meanings or whether its XML has chunk ports.
                let map = crate::genicam::NodeMap::parse(concat!(
                    "<RegisterDescription><Category Name='Root'><pFeature>Counter</pFeature></Category>",
                    "<IntReg Name='Counter'><pPort>Chunk</pPort><Address>0</Address><Length>8</Length><AccessMode>RO</AccessMode><Sign>Unsigned</Sign><Endianess>BigEndian</Endianess></IntReg>",
                    "<Port Name='Chunk'><ChunkID>3e8</ChunkID></Port></RegisterDescription>"
                )).unwrap();
                let values = map.chunk_values(&chunks, &crate::genicam::ChunkContext::default());
                assert_eq!(values["values"]["Counter"], u64::MAX);
                assert_eq!(values["errors"], serde_json::json!({}));
                assert_eq!(chunks.payload.as_ref(), payload);
            }
        }
    }

    #[test]
    fn legacy_chunk_capture_rejects_inconsistent_envelopes_and_recovers() {
        for mode in 0..9 {
            let (mut metadata, mut payload, mut tail) = legacy_chunk_fixture();
            match mode {
                0 => tail[4..8].copy_from_slice(&41u32.to_be_bytes()),
                1 => tail[4..8].copy_from_slice(&43u32.to_be_bytes()),
                2 => tail[4..8].copy_from_slice(&24u32.to_be_bytes()),
                3 => tail[8..12].copy_from_slice(&3u32.to_be_bytes()),
                4 => payload[22..26].copy_from_slice(&17u32.to_be_bytes()),
                5 => payload[38..42].copy_from_slice(&u32::MAX.to_be_bytes()),
                6 => {
                    tail[2..4].copy_from_slice(&0x4001u16.to_be_bytes());
                    tail[4..8].copy_from_slice(&2u32.to_be_bytes());
                    tail[8..12].copy_from_slice(&42u32.to_be_bytes());
                }
                7 => metadata[2..4].copy_from_slice(&0x4005u16.to_be_bytes()),
                8 => {
                    drop(payload.drain(18..26));
                    tail[4..8].copy_from_slice(&34u32.to_be_bytes());
                }
                _ => unreachable!(),
            }
            let mut assembler = Assembler::new(42, 44, 0);
            let attempt = std::iter::once(gvsp(7, 0, 1, false, &metadata))
                .chain(payload.chunks(8).enumerate().map(|(index, bytes)| {
                    gvsp(7, u32::try_from(index + 1).unwrap(), 3, false, bytes)
                }))
                .chain(std::iter::once(gvsp(
                    7,
                    u32::try_from(payload.len().div_ceil(8) + 1).unwrap(),
                    2,
                    false,
                    &tail,
                )))
                .try_for_each(|packet| {
                    assembler
                        .push(&packet)
                        .map(|frame| assert!(frame.is_none()))
                });
            let error = attempt.unwrap_err();
            let expected = match mode {
                0..=2 => "legacy chunk payload length or image boundary",
                3 => "trailer height",
                4 => "chunk overlaps image data",
                5 => "chunk length exceeds payload",
                6 => "payload types disagree",
                7 => "unsupported GVSP payload type",
                8 => "total-length chunk payload lacks its first image tag",
                _ => unreachable!(),
            };
            assert!(
                error.to_string().contains(expected),
                "mode {mode}: {error:#}"
            );
            assert_eq!(
                crate::types::classify(&error).kind,
                if mode == 7 {
                    crate::types::ErrorKind::Unsupported
                } else {
                    crate::types::ErrorKind::OperationFailed
                }
            );
            assert!(assembler.ready.is_empty());
            // The malformed block is retired; the next ordinary frame works.
            assert!(
                assembler
                    .push(&gvsp(8, 0, 1, false, &leader(3, 2, MONO8)))
                    .unwrap()
                    .is_none()
            );
            assert!(
                assembler
                    .push(&gvsp(8, 1, 3, false, &[1, 2, 3, 4, 5, 6]))
                    .unwrap()
                    .is_none()
            );
            let recovered = assembler
                .push(&gvsp(8, 2, 2, false, &trailer(2)))
                .unwrap()
                .unwrap();
            assert_eq!(recovered.data.as_ref(), [1, 2, 3, 4, 5, 6]);
            assert!(recovered.chunks.is_none());
        }
        for size in [8, 12, 15] {
            let (_, _, tail) = legacy_chunk_fixture();
            ImageTrailer::parse(7, &tail[..size]).unwrap_err();
        }
    }

    #[test]
    fn chunk_capture_preserves_wire_padding_and_big_endian_pixels() {
        let mut metadata = leader(3, 2, MONO16);
        metadata[2..4].copy_from_slice(&0x4001u16.to_be_bytes());
        metadata[32..34].copy_from_slice(&2u16.to_be_bytes());
        metadata[34..36].copy_from_slice(&2u16.to_be_bytes());
        let samples = [0x0102u16, 0x0304, 0x0506, 0x0708, 0x090a, 0x0b0c];
        let mut wire = Vec::new();
        for row in samples.chunks(3) {
            wire.extend(row.iter().flat_map(|value| value.to_be_bytes()));
            wire.extend([0xfa, 0xfb]);
        }
        wire.extend([0xfc, 0xfd]);
        wire.extend(0x1122_3344_5566_7788u64.to_be_bytes());
        wire.extend(99u32.to_be_bytes());
        wire.extend(8u32.to_be_bytes());
        let mut tail = vec![0, 0, 0x40, 1];
        tail.extend(2u32.to_be_bytes());
        tail.extend(16u32.to_be_bytes());
        tail.extend(7u32.to_be_bytes());
        let mut assembler = Assembler::new(wire.len(), 44, 0);
        assembler.big_endian = true;
        assembler.push(&gvsp(4, 0, 1, false, &metadata)).unwrap();
        for (index, packet) in wire.chunks(8).enumerate() {
            assembler
                .push(&gvsp(
                    4,
                    u32::try_from(index + 1).unwrap(),
                    3,
                    false,
                    packet,
                ))
                .unwrap();
        }
        let frame = assembler
            .push(&gvsp(4, 6, 2, false, &tail))
            .unwrap()
            .unwrap();
        assert_eq!(
            frame.data.to_vec(),
            samples
                .iter()
                .flat_map(|value| value.to_le_bytes())
                .collect::<Vec<_>>()
        );
        let chunks = frame.chunks.unwrap();
        assert_eq!(&*chunks.payload, &wire);
        assert_eq!(chunks.image_bytes, 18);
        let layout = chunks.image_layout.unwrap();
        assert_eq!(
            (layout.row_bytes, layout.row_stride, layout.padding_y),
            (6, 8, 2)
        );
        assert_eq!(layout.byte_order, Some(ByteOrder::Big));
    }
    #[test]
    fn native_coord3d_preserves_float_words_and_the_original_chunk_payload() {
        use crate::frame::COORD3D_ABC32F;

        let words = [
            0x3f80_0000u32,
            0x8000_0000,
            0x7eff_ffff,
            0x7fc0_2345,
            0x0000_0000,
            0xbf80_0000,
            0x4030_2010,
            0x8000_0001,
            0xffc0_6789,
            0x0000_0001,
            0x3d01_2345,
            0xbe98_7654,
        ];
        let normalized: Vec<u8> = words.iter().flat_map(|word| word.to_le_bytes()).collect();
        for extended in [false, true] {
            for big in [false, true] {
                for chunk_length in [None, Some(false), Some(true)] {
                    let mut head = leader(2, 2, COORD3D_ABC32F);
                    head[32..34].copy_from_slice(&4u16.to_be_bytes());
                    head[34..36].copy_from_slice(&2u16.to_be_bytes());
                    let mut tail = trailer(2);
                    let mut wire = Vec::new();
                    for row in words.as_chunks::<6>().0 {
                        for word in row {
                            wire.extend(if big {
                                word.to_be_bytes()
                            } else {
                                word.to_le_bytes()
                            });
                        }
                        wire.extend([0xfa, 0xfb, 0xfc, 0xfd]);
                    }
                    wire.extend([0xee, 0xef]);
                    let image_bytes = u32::try_from(wire.len()).unwrap();
                    if let Some(total) = chunk_length {
                        head[2] = 0x40;
                        tail[2] = 0x40;
                        wire.extend(1u32.to_be_bytes());
                        wire.extend(image_bytes.to_be_bytes());
                        wire.extend(0x0123_4567_89ab_cdefu64.to_be_bytes());
                        wire.extend(0x0a5a_0001u32.to_be_bytes());
                        wire.extend(8u32.to_be_bytes());
                        let wire_bytes = u32::try_from(wire.len()).unwrap();
                        let reported = if total {
                            wire_bytes
                        } else {
                            wire_bytes - image_bytes
                        };
                        tail.extend(reported.to_be_bytes());
                        tail.extend(7u32.to_be_bytes());
                    }
                    let mut assembler =
                        Assembler::new(wire.len(), if extended { 80 } else { 68 }, 1_000_000);
                    assembler.big_endian = big;
                    assembler.push(&gvsp(15, 0, 1, extended, &head)).unwrap();
                    for (index, bytes) in wire.chunks(32).enumerate() {
                        assert!(
                            assembler
                                .push(&gvsp(
                                    15,
                                    u32::try_from(index + 1).unwrap(),
                                    3,
                                    extended,
                                    bytes
                                ))
                                .unwrap()
                                .is_none()
                        );
                    }
                    let id = u32::try_from(wire.len().div_ceil(32) + 1).unwrap();
                    let frame = assembler
                        .push(&gvsp(15, id, 2, extended, &tail))
                        .unwrap()
                        .unwrap();
                    assert_eq!(
                        (frame.width, frame.height, frame.pixel_format),
                        (2, 2, COORD3D_ABC32F)
                    );
                    assert_eq!(frame.data.as_ref(), normalized);
                    assert_eq!(frame.device_timestamp.unwrap().ticks, 1234);
                    assert_eq!(frame.device_ts_ns, Some(1_234_000));
                    assert_eq!(frame.chunks.is_some(), chunk_length.is_some());
                    if let Some(chunks) = frame.chunks {
                        let layout = chunks.image_layout.unwrap();
                        assert_eq!(
                            (layout.row_bytes, layout.row_stride, layout.padding_y),
                            (24, 28, 2)
                        );
                        assert_eq!(
                            layout.byte_order,
                            Some(if big {
                                ByteOrder::Big
                            } else {
                                ByteOrder::Little
                            })
                        );
                        assert_eq!(chunks.payload.as_ref(), wire);
                        assert_eq!(chunks.image_bytes, 58);
                        assert_eq!(
                            chunks
                                .entries
                                .iter()
                                .map(|chunk| (chunk.id, chunk.offset, chunk.length))
                                .collect::<Vec<_>>(),
                            [(1, 0, 58), (0x0a5a_0001, 66, 8)]
                        );
                    }
                }
            }
        }
        let profile = ImageLeader::parse(&leader(8, 1, COORD3D_ABC32F), 96).unwrap();
        assert_eq!(profile.row_bytes, 96);
        ImageLeader::parse(&leader(2, 2, COORD3D_ABC32F), 47).unwrap_err();
        ImageLeader::parse(&leader(2, 2, 0x0260_00c1), 48).unwrap_err();
        ImageLeader::parse(&leader(2, 2, MONO8), 4).unwrap();
    }

    #[test]
    fn native_coord3d_ac16_profiles_preserve_each_coordinate_word() {
        const AC16: u32 = 0x0220_00bb;
        let expected = [0x34, 0x12, 0xdc, 0xfe, 0xff, 0xff, 0x01, 0x80];
        for (big, wire) in [
            (false, expected),
            (true, [0x12, 0x34, 0xfe, 0xdc, 0xff, 0xff, 0x80, 0x01]),
        ] {
            let mut assembler = Assembler::new(wire.len(), 56, 1_000_000);
            assembler.big_endian = big;
            assembler
                .push(&gvsp(19, 0, 1, true, &leader(2, 1, AC16)))
                .unwrap();
            assert!(
                assembler
                    .push(&gvsp(19, 1, 3, true, &wire))
                    .unwrap()
                    .is_none()
            );
            let frame = assembler
                .push(&gvsp(19, 2, 2, true, &trailer(1)))
                .unwrap()
                .unwrap();
            assert_eq!(
                (frame.width, frame.height, frame.pixel_format),
                (2, 1, AC16)
            );
            assert_eq!(frame.data.as_ref(), expected);
            assert_eq!(frame.device_ts_ns, Some(1_234_000));
        }
    }

    #[test]
    fn total_length_chunk_trailers_require_a_valid_first_image_tag() {
        for tagged in [false, true] {
            let mut head = leader(8, 1, MONO8);
            head[2] = 0x40;
            let mut wire = image(8, 1);
            if tagged {
                wire.extend(1u32.to_be_bytes());
                wire.extend(8u32.to_be_bytes());
            }
            wire.extend([3, 4, 5, 6]);
            wire.extend(9u32.to_be_bytes());
            wire.extend(4u32.to_be_bytes());
            let mut tail = trailer(1);
            tail[2] = 0x40;
            tail.extend(u32::try_from(wire.len()).unwrap().to_be_bytes());
            tail.extend(1u32.to_be_bytes());
            let mut assembler = Assembler::new(wire.len(), 68, 0);
            assembler.push(&gvsp(5, 0, 1, false, &head)).unwrap();
            assembler.push(&gvsp(5, 1, 3, false, &wire)).unwrap();
            let result = assembler.push(&gvsp(5, 2, 2, false, &tail));
            if tagged {
                let frame = result.unwrap().unwrap();
                assert_eq!(frame.data.as_ref(), image(8, 1));
                assert_eq!(frame.chunks.unwrap().payload.as_ref(), wire);
            } else {
                let error = result.unwrap_err();
                assert!(
                    error.to_string().contains("lacks its first image tag"),
                    "{error}"
                );
                assert!(assembler.ready.is_empty());
            }
        }
    }

    #[test]
    fn receive_buffer_requests_halve_on_errors_and_note_small_buffers() {
        let mut sizes = Vec::new();
        request_receive_buffer(1 << 20, |size| {
            sizes.push(size);
            (size <= 2 << 20)
                .then_some(())
                .ok_or_else(|| ErrorKind::Other.into())
        });
        assert_eq!(sizes, [8 << 20, 4 << 20, 2 << 20]);
        sizes.clear();
        request_receive_buffer(100 << 20, |size| {
            sizes.push(size);
            Err(ErrorKind::Other.into())
        });
        assert_eq!((sizes[0], sizes[sizes.len() - 1]), (64 << 20, 256 << 10));
        assert!(receive_buffer_note(64 << 20, 1 << 20).is_none());
        assert!(
            receive_buffer_note(1 << 20, 1 << 20)
                .is_some_and(|note| note.contains("less than two"))
        );
    }
    /// A receive buffer past `net.core.rmem_max` where the process may ask
    /// for one (root, or `CAP_NET_ADMIN`), else as large as the limit allows.
    #[test]
    fn receive_buffers_are_forced_past_the_limit_where_allowed() {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let wanted = 8 << 20;
        set_receive_buffer(&socket, wanted).unwrap();
        let got = socket2::SockRef::from(&socket).recv_buffer_size().unwrap();
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd as _;
            let probe = UdpSocket::bind("127.0.0.1:0").unwrap();
            let reference = socket2::SockRef::from(&probe);
            reference.set_recv_buffer_size(wanted).unwrap();
            let ordinary = reference.recv_buffer_size().unwrap();
            let size = libc::c_int::try_from(wanted).unwrap();
            // Root inside a container can lack CAP_NET_ADMIN, while a non-root
            // process can hold it. Probe the operation rather than the UID.
            // SAFETY: probe stays live and setsockopt reads one initialized int.
            let forced = unsafe {
                libc::setsockopt(
                    probe.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_RCVBUFFORCE,
                    (&raw const size).cast(),
                    libc::socklen_t::try_from(size_of::<libc::c_int>()).unwrap(),
                )
            } == 0;
            if forced {
                let capped = crate::host::memory_budget().map_or(wanted, |budget| {
                    wanted.min(usize::try_from(budget / 16).unwrap_or(usize::MAX))
                });
                // Production caps its forced request to the host's memory share.
                // Linux doubles that request for its bookkeeping.
                assert!(got >= capped, "{got} < {capped}");
            } else {
                assert_eq!(
                    got, ordinary,
                    "unprivileged socket must use the ordinary fallback"
                );
            }
        }
        assert!(got > 0);
    }
    #[test]
    fn packet_size_ladder_probes_few_sizes() {
        let run = |path: u32, cap, mtu_known| {
            let mut probes = Vec::new();
            let size = ladder(
                |size| {
                    probes.push(size);
                    (size <= path).then_some(size)
                },
                cap,
                mtu_known,
            );
            (size, probes)
        };
        assert_eq!(run(9000, 9000, false), (Some(9000), vec![9000]));
        assert_eq!(run(1500, 9000, false), (Some(1500), vec![9000, 1500]));
        assert_eq!(run(1500, 1500, true), (Some(1500), vec![1500]));
        assert_eq!(run(1400, 1400, true), (Some(1400), vec![1400]));
        assert_eq!(run(0, 9000, false), (None, vec![9000, 1500, 576]));
        let (size, probes) = run(1400, 9000, false);
        assert!(size.is_some_and(|size| (1350..=1400).contains(&size)) && probes.len() <= 9);
        let (size, probes) = run(4000, 9000, true);
        assert!(size.is_some_and(|size| (3900..=4000).contains(&size)) && probes.len() <= 8);
        let (size, probes) = run(u32::MAX - 4, u32::MAX, true);
        assert!(size.is_some_and(|size| size > u32::MAX / 2 && size <= u32::MAX - 4));
        assert!(probes.len() <= 8);
    }
    #[test]
    fn packet_size_probes_against_fake_camera() {
        let path = |mtu| move |camera: &mut FakeCamera| camera.path_mtu = mtu;
        assert_eq!(negotiate(path(9000), 9000, false), Some(9000));
        assert_eq!(negotiate(path(1500), 9000, false), Some(1500));
        assert!(
            negotiate(path(1400), 9000, false).is_some_and(|size| (1350..=1400).contains(&size))
        );
        let clamping = |camera: &mut FakeCamera| {
            camera.path_mtu = 9000;
            camera.clamp = Some(8192);
        };
        assert_eq!(negotiate(clamping, 9000, false), Some(8192));
        let rejecting = |camera: &mut FakeCamera| {
            camera.path_mtu = 9000;
            camera.reject_above = Some(4000);
        };
        assert_eq!(negotiate(rejecting, 9000, false), Some(1500));
        assert!(negotiate(rejecting, 9000, true).is_some_and(|size| (3900..=4000).contains(&size)));
        let silent = |camera: &mut FakeCamera| {
            camera.test_packets = false;
            camera.stores_flags = true;
        };
        assert_eq!(negotiate(silent, 9000, false), None);
        let stale = |camera: &mut FakeCamera| {
            camera.path_mtu = 1400;
            camera.stale = true;
        };
        assert!(negotiate(stale, 1500, true).is_some_and(|size| (1350..=1400).contains(&size)));
    }
    #[test]
    fn packet_size_probe_accepts_three_packets_delayed_by_scheduler_jitter() {
        assert_eq!(
            negotiate(
                |camera| {
                    camera.path_mtu = 1500;
                    camera.test_packet_delay = Duration::from_millis(30);
                },
                1500,
                true
            ),
            Some(1500)
        );
    }

    #[test]
    fn network_probe_requires_three_read_only_unicast_responses() {
        let fake = Fake::new(|_| {});
        assert_eq!(
            probe_network_at(fake.address, Duration::from_millis(100)).unwrap(),
            Ipv4Addr::LOCALHOST
        );
        let camera = fake.camera.lock().unwrap();
        assert_eq!(camera.register_reads, [GVCP_CAPABILITY; 3]);
        assert!(
            camera.register_writes.is_empty(),
            "a scan must not acquire camera control"
        );
    }
    #[test]
    fn network_probe_rejects_unreadable_and_silent_cameras_within_deadline() {
        let fake = Fake::new(|camera| camera.unreadable.push(GVCP_CAPABILITY));
        probe_network_at(fake.address, Duration::from_millis(100)).unwrap_err();
        assert_eq!(
            fake.camera.lock().unwrap().register_writes,
            [] as [(u32, u32); 0]
        );
        let silent = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let SocketAddr::V4(address) = silent.local_addr().unwrap() else {
            unreachable!()
        };
        let started = Instant::now();
        probe_network_at(address, Duration::from_millis(40)).unwrap_err();
        assert!(started.elapsed() < Duration::from_millis(500));
    }
    #[test]
    fn one_test_packet_never_proves_a_reliable_stream_path() {
        assert_eq!(
            negotiate(
                |camera| {
                    camera.path_mtu = 9000;
                    camera.test_packet_budget = Some(1);
                },
                9000,
                false
            ),
            None
        );
    }
    #[test]
    fn network_optimization_retests_negative_cache_and_restores_destination() {
        let saved_address = u32::from(Ipv4Addr::new(192, 0, 2, 44));
        let flags = SCPS_NO_FRAGMENT | SCPS_BIG_ENDIAN;
        let fake = Fake::new(|camera| {
            camera.path_mtu = 1500;
            camera.test_packets = true;
            camera.registers.insert(STREAM_ADDRESS, saved_address);
            camera.registers.insert(STREAM_PORT, 7123);
            camera.registers.insert(STREAM_PACKET_SIZE, flags | 576);
        });
        let mut gige = connect(fake.address, Duration::from_millis(200)).unwrap();
        gige.packet_size = PacketSize::Unverified;
        let tuning = gige.optimize_network(8000).unwrap();
        assert_eq!(tuning.packet_size, Some(1500));
        assert_eq!(gige.packet_size, PacketSize::Verified(1500));
        assert!(
            tuning
                .changes
                .iter()
                .any(|change| change.contains("three consecutive"))
        );
        assert_eq!(fake.register(STREAM_ADDRESS), saved_address);
        assert_eq!(fake.register(STREAM_PORT), 7123);
        assert_eq!(fake.register(STREAM_PACKET_SIZE), SCPS_NO_FRAGMENT | 1500);
        assert!(gige.stream.is_none());
    }
    #[test]
    fn failed_network_tests_apply_conservative_settings_without_claiming_verification() {
        let fake = Fake::new(|camera| {
            camera.path_mtu = 9000;
            camera.test_packets = true;
            camera.test_packet_budget = Some(1);
            camera
                .registers
                .insert(STREAM_ADDRESS, u32::from(Ipv4Addr::LOCALHOST));
            camera.registers.insert(STREAM_PORT, 8123);
            camera.registers.insert(STREAM_PACKET_SIZE, 9000);
        });
        let mut gige = connect(fake.address, Duration::from_millis(200)).unwrap();
        let tuning = gige.optimize_network(8000).unwrap();
        assert_eq!(tuning.packet_size, Some(1500));
        assert_eq!(fake.register(STREAM_PACKET_SIZE), 1500);
        assert_eq!(fake.register(STREAM_PORT), 8123);
        assert!(
            tuning
                .warnings
                .iter()
                .any(|warning| warning.contains("unverified"))
        );
        assert_eq!(gige.packet_size, PacketSize::Unverified);
    }
    #[test]
    fn rejected_network_settings_restore_packet_size_and_destination() {
        let flags = SCPS_NO_FRAGMENT | SCPS_BIG_ENDIAN;
        let fake = Fake::new(|camera| {
            camera.rejected_packet_sizes = vec![1500, 576];
            camera
                .registers
                .insert(STREAM_ADDRESS, u32::from(Ipv4Addr::LOCALHOST));
            camera.registers.insert(STREAM_PORT, 8123);
            camera.registers.insert(STREAM_PACKET_SIZE, flags | 1400);
        });
        let mut gige = connect(fake.address, Duration::from_millis(200)).unwrap();
        gige.optimize_network(8000).unwrap_err();
        assert_eq!(
            fake.register(STREAM_ADDRESS),
            u32::from(Ipv4Addr::LOCALHOST)
        );
        assert_eq!(fake.register(STREAM_PORT), 8123);
        assert_eq!(fake.register(STREAM_PACKET_SIZE), flags | 1400);
        assert_eq!(gige.packet_size, PacketSize::Untested);
    }
    #[test]
    fn start_clears_only_big_endian_drains_test_packets_and_caches_the_size() {
        let flags = SCPS_NO_FRAGMENT | SCPS_BIG_ENDIAN;
        let fake = Fake::new(|camera| {
            camera.path_mtu = 9000;
            camera.test_packets = true;
            camera.stale = true;
            camera
                .registers
                .insert(STREAM_PACKET_SIZE, SCPS_FIRE | flags | 576);
        });
        let mut gige = connect(fake.address, Duration::from_millis(200)).unwrap();
        gige.start(8000).unwrap();
        let kept = SCPS_NO_FRAGMENT;
        assert_eq!(
            fake.camera.lock().unwrap().scps_writes,
            [
                SCPS_FIRE | kept | 9000,
                SCPS_FIRE | kept | 9000,
                SCPS_FIRE | kept | 9000,
                kept | 9000
            ]
        );
        assert!(!gige.stream.as_ref().unwrap().assembler.big_endian);
        let socket = &gige.stream.as_ref().unwrap().socket;
        socket.set_nonblocking(true).unwrap();
        socket.recv(&mut vec![0; 65_536]).unwrap_err();
        socket.set_nonblocking(false).unwrap();
        assert_eq!(gige.stats().unwrap().packet_size, Some(9000));
        gige.stop().unwrap();
        gige.start(8000).unwrap();
        assert_eq!(fake.camera.lock().unwrap().scps_writes.len(), 8);
    }
    #[test]
    fn start_without_test_packets_keeps_1500_and_notes_the_firewall() {
        let fake = Fake::new(|camera| {
            camera.stores_flags = true;
            camera
                .registers
                .insert(STREAM_PACKET_SIZE, SCPS_FIRE | 1400);
        });
        let mut gige = connect(fake.address, Duration::from_millis(200)).unwrap();
        gige.start(8000).unwrap();
        let stats = gige.stats().unwrap();
        assert_eq!(stats.packet_size, Some(1500));
        assert!(
            stats
                .notes
                .iter()
                .any(|note| note.contains("allow inbound UDP from 127.0.0.1")),
            "{:?}",
            stats.notes
        );
        assert_eq!(fake.camera.lock().unwrap().scps_writes.last(), Some(&1500));
        gige.stop().unwrap();
        let probes = fake.camera.lock().unwrap().scps_writes.len();
        gige.start(8000).unwrap();
        assert_eq!(fake.camera.lock().unwrap().scps_writes[probes..], [1500]);
    }
    #[test]
    fn stream_setup_opens_the_firewall_path_from_the_stream_source_port() {
        for supported in [true, false] {
            let fake = Fake::new(|camera| {
                camera.path_mtu = 9000;
                camera.test_packets = true;
                if supported {
                    camera.registers.insert(STREAM_CAPABILITY, 0x8000_0000);
                }
            });
            let source = u32::from(fake.stream.local_addr().unwrap().port());
            fake.camera
                .lock()
                .unwrap()
                .registers
                .insert(STREAM_SOURCE_PORT, source);
            fake.stream
                .set_read_timeout(Some(Duration::from_millis(200)))
                .unwrap();
            let mut gige = connect(fake.address, Duration::from_secs(1)).unwrap();
            gige.packet_size = PacketSize::Verified(1500);
            gige.start(8000).unwrap();
            let stream = gige.stream.as_mut().unwrap();
            let port = stream.socket.local_addr().unwrap().port();
            stream.next_traversal = Instant::now();
            gige.next_frame(Duration::from_millis(20)).unwrap_err();
            let mut buffer = [0; 64];
            for _ in 0..2 {
                let received = fake.stream.recv_from(&mut buffer);
                assert_eq!(
                    received.ok().map(|(_, from)| from.port()),
                    supported.then_some(port)
                );
            }
            assert_eq!(gige.stats().unwrap().packet_size, Some(1500));
            // A wait longer than the refresh interval does not delay it.
            let stream = gige.stream.as_mut().unwrap();
            stream.next_traversal = Instant::now() + Duration::from_millis(50);
            thread::scope(|scope| {
                scope.spawn(|| {
                    gige.next_frame(Duration::from_millis(500)).unwrap_err();
                });
                let received = fake.stream.recv_from(&mut buffer);
                assert_eq!(
                    received.ok().map(|(_, from)| from.port()),
                    supported.then_some(port)
                );
            });
            // The network test opens the path too; the source port is read
            // once per connection.
            gige.stop().unwrap();
            gige.start(8000).unwrap();
            gige.stop().unwrap();
            gige.optimize_network(8000).unwrap();
            for _ in 0..2 {
                let received = fake.stream.recv_from(&mut buffer);
                assert_eq!(
                    received.ok().map(|(length, _)| length),
                    supported.then_some(4)
                );
            }
            let camera = fake.camera.lock().unwrap();
            let reads = camera.register_reads.iter();
            assert_eq!(reads.filter(|read| **read == STREAM_CAPABILITY).count(), 1);
        }
    }
    #[test]
    fn fake_camera_resends_lost_packets() {
        for (status, extended) in [(0x0100, false), (0x0000, false), (0x0100, true)] {
            let (fake, mut gige) = streaming(|camera| {
                camera.registers.insert(GVCP_CAPABILITY, 0x4);
                camera.resend_status = status;
            });
            let block = if extended { 0x1_0000_0001 } else { 1 };
            fake.send(block, extended, &[3, 7]);
            let frame = gige.next_frame(Duration::from_secs(2)).unwrap();
            assert_eq!(
                (frame.id, frame.data.to_vec()),
                (block, image(WIDTH, HEIGHT))
            );
            gige.next_frame(Duration::from_millis(50)).unwrap_err();
            let port = gige
                .stream
                .as_ref()
                .unwrap()
                .socket
                .local_addr()
                .unwrap()
                .port();
            let mut requests = fake.camera.lock().unwrap().requests.clone();
            requests.sort_by_key(|(packet, _)| be32(packet, 12));
            requests.dedup_by_key(|(packet, _)| be32(packet, 12));
            assert_eq!(requests.len(), 2);
            for ((packet, from), id) in requests.iter().zip([3, 7]) {
                assert_eq!(*from, port);
                assert_eq!(
                    packet[..6],
                    [
                        0x42,
                        if extended { 0x10 } else { 0 },
                        0,
                        0x40,
                        0,
                        if extended { 20 } else { 12 }
                    ]
                );
                assert_eq!(
                    *packet,
                    resend_packet(be16(packet, 6), block, extended, id, id)
                );
            }
            assert_ne!(requests[0].0[6..8], requests[1].0[6..8]);
            let stats = gige.stats().unwrap();
            assert!(stats.resend_requested >= 2);
            assert_eq!(
                (
                    stats.resend_recovered,
                    stats.incomplete_frames,
                    stats.packets_received
                ),
                (1, 0, 8)
            );
        }
    }
    #[test]
    fn fake_stream_sends_leave_camera_control_available() {
        let (fake, mut gige) = streaming(|_| {});
        let mut control = Control::connect(fake.address, Duration::from_secs(1)).unwrap();
        let mut sent = 0;
        fake.send_with(1, false, &[], |packet, destination| {
            // This read must finish while stream delivery is paused here.
            // Keeping the fake state locked across send would starve GVCP.
            assert_ne!(control.read_register(CONTROL_PRIVILEGE).unwrap() & 3, 0);
            fake.stream.send_to(packet, destination).unwrap();
            sent += 1;
        });
        assert!(sent > 0);
        assert_eq!(gige.next_frame(Duration::from_secs(1)).unwrap().id, 1);
    }
    #[test]
    fn frames_survive_a_worker_stall_while_packets_wait_in_the_socket() {
        let (fake, mut gige) = streaming(|camera| {
            camera.registers.insert(GVCP_CAPABILITY, 0x4);
        });
        fake.send(1, false, &[4, 5, 6, 7]);
        gige.next_frame(Duration::from_millis(5)).unwrap_err();
        let before = gige.stats().unwrap().resend_requested;
        thread::sleep(FRAME_IDLE + Duration::from_millis(50));
        let (packets, destination) = {
            let camera = fake.camera.lock().unwrap();
            (camera.frames[&1][4..].to_vec(), camera.destination())
        };
        for packet in packets {
            fake.stream.send_to(&packet, destination).unwrap();
        }
        assert_eq!(gige.next_frame(Duration::from_secs(1)).unwrap().id, 1);
        let stats = gige.stats().unwrap();
        assert_eq!(
            (stats.resend_requested, stats.incomplete_frames),
            (before, 0)
        );
    }
    #[test]
    fn frames_waiting_in_the_socket_are_taken_in_and_the_clock_latched() {
        let (fake, mut gige) = streaming(|camera| {
            camera.registers.insert(GVCP_CAPABILITY, 0x4);
            camera.registers.insert(TIMESTAMP_VALUE_HIGH, 1);
            camera.registers.insert(TIMESTAMP_VALUE_LOW, 500);
        });
        fake.send(1, false, &[]);
        fake.send(2, false, &[]);
        // The third is still being read out.
        fake.send(3, false, &[4, 5, 6, 7]);
        thread::sleep(Duration::from_millis(50));
        assert_eq!(gige.buffered_frames(), 3);
        for id in 1..=2 {
            assert_eq!(gige.next_frame(Duration::from_millis(100)).unwrap().id, id);
        }
        // Read at a 1 GHz tick, as the camera told at connect.
        gige.timestamp_frequency = 1_000_000_000;
        assert_eq!(gige.device_time_ns(), Some((1 << 32) + 500));
        assert!(
            fake.camera
                .lock()
                .unwrap()
                .register_writes
                .contains(&(TIMESTAMP_CONTROL, TIMESTAMP_LATCH))
        );
        gige.timestamp_frequency = 0;
        assert_eq!(gige.device_time_ns(), None);
    }
    #[test]
    fn heartbeats_recover_only_before_the_old_proof_deadline() {
        let fake = Fake::new(|camera| {
            camera.registers.insert(CONTROL_PRIVILEGE, 2);
        });
        let mut control = Control::connect(fake.address, Duration::from_millis(30)).unwrap();
        let (lease, now) = Lease::testing(300, false);
        control.lease = Some(lease);
        let control = Arc::new(Mutex::new(control));
        let heartbeat = Heartbeat::start(control.clone()).unwrap();
        fake.camera
            .lock()
            .unwrap()
            .unreadable
            .push(CONTROL_PRIVILEGE);
        let deadline = Instant::now() + Duration::from_secs(2);
        while fake.camera.lock().unwrap().register_reads.is_empty() {
            assert!(Instant::now() < deadline, "heartbeat did not run");
            thread::sleep(Duration::from_millis(5));
        }
        heartbeat.check().unwrap();
        fake.camera.lock().unwrap().unreadable.clear();
        thread::sleep(Duration::from_millis(60));
        heartbeat.check().unwrap();
        // Hold the I/O lock while advancing proof, so an already sent
        // datagram cannot race the no-future-send assertion.
        let transaction = control.lock().unwrap();
        now.store(1_300_000_000, Ordering::SeqCst);
        let failed = heartbeat.check().unwrap_err();
        assert!(ownership_retired(&failed));
        drop(transaction);
        let reads = fake.camera.lock().unwrap().register_reads.len();
        // A replacement controller reports the same global CCP=2.
        thread::sleep(Duration::from_millis(150));
        assert_eq!(
            heartbeat.check().unwrap_err().to_string(),
            failed.to_string()
        );
        assert_eq!(fake.camera.lock().unwrap().register_reads.len(), reads);
    }
    #[test]
    fn an_idle_stream_sleeps_until_the_timeout_and_polls_a_frame_in_flight() {
        let (fake, mut gige) = streaming(|camera| {
            camera.registers.insert(GVCP_CAPABILITY, 0x4);
        });
        let timeout = |gige: &GigE| gige.stream.as_ref().unwrap().timeout;
        // Waiting for a trigger: one receive for the whole timeout.
        let started = Instant::now();
        let error = gige.next_frame(Duration::from_millis(100)).unwrap_err();
        let elapsed = started.elapsed();
        assert!(
            error.to_string().starts_with("frame receive timed out;"),
            "idle receive failed after {elapsed:?}: {error:#}"
        );
        assert!(
            elapsed >= Duration::from_millis(100),
            "idle receive timed out early after {elapsed:?}: {error:#}"
        );
        // A Windows receive can return a little early, leaving a short final
        // wait that says nothing about how long the first one was.
        if !cfg!(windows) {
            assert!(timeout(&gige) > POLL * 8, "{:?}", timeout(&gige));
        }
        // A frame still arriving is in flight: back to polling for resends.
        fake.send(1, false, &[4, 5, 6, 7]);
        gige.next_frame(Duration::from_millis(5)).unwrap_err();
        assert!(timeout(&gige) <= POLL, "{:?}", timeout(&gige));
        let (packets, destination) = {
            let camera = fake.camera.lock().unwrap();
            (camera.frames[&1][4..].to_vec(), camera.destination())
        };
        for packet in packets {
            fake.stream.send_to(&packet, destination).unwrap();
        }
        assert_eq!(gige.next_frame(Duration::from_secs(1)).unwrap().id, 1);
        gige.next_frame(Duration::from_millis(100)).unwrap_err();
        assert!(timeout(&gige) > POLL * 8, "{:?}", timeout(&gige));
    }
    #[test]
    fn a_leader_armed_for_a_trigger_waits_without_polling() {
        let (fake, mut gige) = streaming(|camera| {
            camera.registers.insert(GVCP_CAPABILITY, 0x4);
        });
        let destination = fake.camera.lock().unwrap().destination();
        let packets = frame_packets(1, false, WIDTH, HEIGHT, 1464);
        fake.stream.send_to(&packets[0], destination).unwrap();
        let error = gige.next_frame(Duration::from_millis(100)).unwrap_err();
        assert!(error.to_string().contains("check trigger mode"), "{error}");
        if !cfg!(windows) {
            let timeout = gige.stream.as_ref().unwrap().timeout;
            assert!(timeout > POLL * 8, "{timeout:?}");
        }
        gige.next_frame(Duration::from_millis(200)).unwrap_err();
        for packet in &packets[1..] {
            fake.stream.send_to(packet, destination).unwrap();
        }
        assert_eq!(gige.next_frame(Duration::from_secs(1)).unwrap().id, 1);
        assert_eq!(
            fake.camera.lock().unwrap().requests,
            [] as [(Vec<u8>, u16); 0]
        );
    }
    #[test]
    fn fake_camera_unavailable_packets_abandon_the_frame() {
        // Gone packets end recovery; not yet available ones are retried.
        for (status, requests) in [(0x800c, 2), (0x8014, 6)] {
            let (fake, mut gige) = streaming(|camera| {
                camera.registers.insert(GVCP_CAPABILITY, 0x4);
                camera.resend_status = status;
            });
            fake.send(1, false, &[3, 7]);
            gige.next_frame(Duration::from_millis(300)).unwrap_err();
            gige.next_frame(Duration::from_millis(100)).unwrap_err();
            let stats = gige.stats().unwrap();
            assert_eq!(
                (
                    stats.resend_requested,
                    stats.resend_recovered,
                    stats.incomplete_frames
                ),
                (requests as u64, 0, 1)
            );
            // The fake camera takes the requests in on its own thread, which
            // a busy runner may not have run since the last one was sent.
            let deadline = Instant::now() + Duration::from_secs(5);
            let seen = loop {
                let seen = fake.camera.lock().unwrap().requests.len();
                if seen >= requests || Instant::now() >= deadline {
                    break seen;
                }
                thread::sleep(Duration::from_millis(5));
            };
            assert_eq!(seen, requests);
        }
    }
    #[test]
    fn unsupported_payloads_fail_without_waiting_for_the_timeout() {
        let (fake, mut gige) = streaming(|_| {});
        let mut jpeg = leader(WIDTH, HEIGHT, MONO8);
        jpeg[2..4].copy_from_slice(&0x0006u16.to_be_bytes());
        let destination = fake.camera.lock().unwrap().destination();
        // A late packet-size test packet is not a reason to give up.
        fake.stream.send_to(&[0; 1472], destination).unwrap();
        let error = gige.next_frame(Duration::from_millis(100)).unwrap_err();
        assert!(error.to_string().contains("timed out"), "{error}");
        fake.stream
            .send_to(&gvsp(1, 0, 1, false, &jpeg), destination)
            .unwrap();
        let started = Instant::now();
        let error = gige.next_frame(Duration::from_secs(5)).unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(error.to_string().contains("JPEG"), "{error}");
        assert_eq!(
            crate::types::error_transport_code(&error),
            Some("unsupported")
        );
    }
    #[test]
    fn big_endian_samples_are_swapped_to_little_endian() {
        // A camera that keeps GevSCPSBigEndian set sends 16-bit samples MSB
        // first.
        let (fake, mut gige) = streaming(|camera| camera.sticky_flags = SCPS_BIG_ENDIAN);
        let writes = fake.camera.lock().unwrap().scps_writes.clone();
        assert!(writes.iter().all(|value| value & SCPS_BIG_ENDIAN == 0));
        let notes = gige.stats().unwrap().notes;
        assert!(notes.iter().any(|note| note.contains("GevSCPSBigEndian")));
        let destination = fake.camera.lock().unwrap().destination();
        let mut packets = frame_packets(1, false, WIDTH, HEIGHT, 1464);
        packets[0] = gvsp(1, 0, 1, false, &leader(WIDTH / 2, HEIGHT, MONO16));
        for packet in &packets {
            fake.stream.send_to(packet, destination).unwrap();
        }
        let mut expected = image(WIDTH, HEIGHT);
        for sample in expected.as_chunks_mut::<2>().0 {
            sample.swap(0, 1);
        }
        assert_eq!(
            gige.next_frame(Duration::from_secs(1)).unwrap().data,
            expected
        );

        assert!(big_endian_mono16(
            "Point Grey Research",
            "Blackfly BFLY-PGE-31S4M"
        ));
        assert!(big_endian_mono16(
            "Point Grey Research",
            "Blackfly BFLY-PGE-14S2C"
        ));
        assert!(!big_endian_mono16(
            "Point Grey Research",
            "Blackfly BFLY-PGE-23S6C"
        ));
        assert!(!big_endian_mono16("FLIR", "Blackfly BFLY-PGE-31S4M"));
        let mono12 = 0x0110_0005;
        let rgb16 = 0x0230_0033;
        let yuv422 = 0x0210_0032;
        let rgba12p = 0x0230_0062;
        let rgba16 = 0x0240_0064;
        let coord3d_ac16 = 0x0220_00bb;
        let coord3d_c32f = 0x0120_00bf;
        for (pixel, width, big_endian, mono16, sample) in [
            (MONO16, 6, false, true, 2),
            (mono12, 6, false, true, 1),
            (mono12, 6, true, false, 2),
            (rgb16, 2, true, false, 2),
            (MONO8, 12, true, true, 1),
            (yuv422, 6, true, false, 1),
            (rgba12p, 2, true, false, 1),
            (rgba16, 1, true, false, 2),
            (coord3d_ac16, 3, true, false, 2),
            (coord3d_c32f, 3, true, false, 4),
            (0x0210_00a5, 6, true, false, 1),
            (0x0210_00a6, 6, true, false, 1),
            (0x0220_00a7, 3, true, false, 2),
            (0x0220_00a9, 3, true, false, 2),
            (0x0220_00ab, 3, true, false, 2),
            (0x0220_00ad, 3, true, false, 2),
            // Two-component 32-bit coordinates share the 0x0240 size code
            // with four-component 16-bit color: the exact format wins.
            (0x0240_00c2, 1, true, false, 4),
            (0x0240_00c3, 1, true, false, 4),
        ] {
            let mut assembler = Assembler::new(12, 48, 0);
            assembler.big_endian = big_endian;
            assembler.big_endian_mono16 = mono16;
            let wire: Vec<u8> = (1..=12).collect();
            assembler
                .push(&gvsp(1, 0, 1, false, &leader(width, 1, pixel)))
                .unwrap();
            assembler.push(&gvsp(1, 1, 3, false, &wire)).unwrap();
            let frame = assembler
                .push(&gvsp(1, 2, 2, false, &trailer(1)))
                .unwrap()
                .unwrap();
            let mut expected = wire[..frame.data.len()].to_vec();
            expected.chunks_exact_mut(sample).for_each(<[u8]>::reverse);
            assert_eq!(frame.data, expected, "{pixel:08x}");
        }
    }
    #[test]
    fn fake_camera_without_resend_capability_gets_no_requests() {
        let (fake, mut gige) = streaming(|_| {});
        assert!(!gige.resend);
        let destination = fake.camera.lock().unwrap().destination();
        fake.gvcp
            .send_to(
                &gvsp(9, 0, 1, false, &leader(WIDTH, HEIGHT, MONO8)),
                destination,
            )
            .unwrap();
        fake.send(1, false, &[3]);
        let error = gige
            .next_frame(Duration::from_millis(400))
            .unwrap_err()
            .to_string();
        assert!(error.contains("timed out"), "{error}");
        fake.send(2, false, &[]);
        assert_eq!(gige.next_frame(Duration::from_secs(1)).unwrap().id, 2);
        assert_eq!(
            fake.camera.lock().unwrap().requests,
            [] as [(Vec<u8>, u16); 0]
        );
        let stats = gige.stats().unwrap();
        assert_eq!(
            (
                stats.incomplete_frames,
                stats.lost_frames,
                stats.packets_received
            ),
            (1, 0, 15)
        );
        gige.stop().unwrap();
        gige.start((WIDTH * HEIGHT) as usize).unwrap();
        assert_eq!(gige.stats().unwrap().packets_received, 15);
    }
    #[test]
    fn connect_reads_resend_capability_and_retains_the_actual_heartbeat_timeout() {
        for (capability, unreadable, resend) in
            [(0x4, false, true), (0x0, false, false), (0x4, true, false)]
        {
            let fake = Fake::new(|camera| {
                camera.registers.insert(HEARTBEAT_TIMEOUT, 1000);
                camera.registers.insert(GVCP_CAPABILITY, capability);
                if unreadable {
                    camera.unreadable.push(GVCP_CAPABILITY);
                }
            });
            let gige = connect(fake.address, Duration::from_millis(200)).unwrap();
            assert_eq!(gige.resend, resend);
            assert_eq!(fake.register(HEARTBEAT_TIMEOUT), 1000);
            assert_eq!(gige.stats(), Some(TransportStats::default()));
            drop(gige);
            assert_eq!(
                (
                    fake.register(HEARTBEAT_TIMEOUT),
                    fake.register(CONTROL_PRIVILEGE)
                ),
                (1000, 0)
            );
        }
    }

    /// Every packet of `packets` taken in by `assembler`, as the stream
    /// takes them: with `scatter`, a packet the assembler predicts is split
    /// as recvmsg splits it, into the header, its predicted place in the
    /// frame and the overflow. What each push returned, and the assembler's
    /// state after it.
    fn outcomes(assembler: &mut Assembler, packets: &[Vec<u8>], scatter: bool) -> Vec<String> {
        let start = Instant::now();
        let (mut scratch, mut overflow) = (Vec::new(), vec![0; 64 * 1024]);
        let mut seen = Vec::new();
        for (i, packet) in packets.iter().enumerate() {
            let now = start + Duration::from_millis(i as u64 * 3);
            let result = match assembler.predict().filter(|_| scatter) {
                Some(slot) => {
                    let mut header = [0xEE; 20];
                    let head = packet.len().min(slot.header);
                    header[..head].copy_from_slice(&packet[..head]);
                    let rest = &packet[head..];
                    let in_slot = rest.len().min(slot.len);
                    let place = assembler.slot_mut(&slot).unwrap();
                    place[..in_slot].copy_from_slice(&rest[..in_slot]);
                    overflow[..rest.len() - in_slot].copy_from_slice(&rest[in_slot..]);
                    let received = (&header[..], packet.len(), false);
                    assembler.landed(&slot, received, &overflow, &mut scratch, now)
                }
                None => assembler.push_counted(packet, now, Site::GvspFallback),
            };
            let result = result.map(|frame| {
                frame.map(|f| (f.id, f.width, f.height, f.device_ts_ns, f.data.to_vec()))
            });
            if i % 7 == 6 {
                assembler.poll(now + FRAME_IDLE / 2);
            }
            let frames: Vec<_> = (assembler.frames.iter())
                .map(|f| {
                    (
                        f.block,
                        f.count,
                        f.bytes,
                        f.end,
                        f.packets,
                        f.received.clone(),
                    )
                })
                .collect();
            seen.push(format!(
                "{:?} {:?} {:?} {:?} {frames:?}",
                result.map_err(|e| format!("{e:#}")),
                assembler.stats,
                assembler.requests,
                assembler.ready.len(),
            ));
        }
        seen
    }

    /// Round `round`'s generated packets, of three frames reordered,
    /// duplicated, lost, failed and cut short in ways `random` picks, and an
    /// assembler for them.
    fn generated(
        random: &mut impl FnMut(usize) -> usize,
        round: usize,
    ) -> (Vec<Vec<u8>>, Assembler) {
        let extended = round.is_multiple_of(3);
        let (width, height) = ([16, 30, 64, 7][round % 4], [4, 9, 3][round % 3]);
        let block_size = [16, 33, 64, 100][random(4)];
        let first = if extended {
            0xffff_fff0 + random(4) as u64
        } else {
            65_530 + random(10) as u64
        };
        let mut packets = Vec::new();
        for frame in 0..3u64 {
            // Standard block IDs wrap past 65535 to 1.
            let block = if extended {
                first + frame
            } else {
                (first + frame - 1) % 65_535 + 1
            };
            let mut frame = frame_packets(block, extended, width, height, block_size);
            // Row and image padding the camera adds.
            if round % 5 == 1 {
                let image: Vec<u8> = image(width, height)
                    .chunks(width as usize)
                    .flat_map(|row| [row, &[0xAB, 0xCD][..]].concat())
                    .chain([7; 3])
                    .collect();
                let mut lead = leader(width, height, MONO8);
                lead[32..34].copy_from_slice(&2u16.to_be_bytes());
                lead[34..36].copy_from_slice(&3u16.to_be_bytes());
                frame = vec![gvsp(block, 0, 1, extended, &lead)];
                for chunk in image.chunks(block_size) {
                    frame.push(gvsp(
                        block,
                        u32::try_from(frame.len()).unwrap(),
                        3,
                        extended,
                        chunk,
                    ));
                }
                frame.push(gvsp(
                    block,
                    u32::try_from(frame.len()).unwrap(),
                    2,
                    extended,
                    &trailer(height),
                ));
            }
            for _ in 0..random(4) {
                let at = random(frame.len());
                match random(8) {
                    // Reordered.
                    0 | 1 if at + 1 < frame.len() => frame.swap(at, at + 1),
                    // A late leader.
                    2 => {
                        let leader = frame.remove(0);
                        frame.insert(at.min(frame.len()), leader);
                    }
                    // A duplicate, later.
                    3 => {
                        let copy = frame[at].clone();
                        frame.insert(random(frame.len() + 1), copy);
                    }
                    // Lost, or resent much later.
                    4 => {
                        let lost = frame.remove(at);
                        if random(2) == 0 {
                            frame.push(lost);
                        }
                    }
                    // An error status from the camera.
                    5 => frame[at][..2].copy_from_slice(&[0x80, 0x01]),
                    // A resend the camera cannot serve yet.
                    6 => {
                        let mut unavailable = frame[at].clone();
                        unavailable[..2].copy_from_slice(&[0x80, 0x14]);
                        unavailable.truncate(if extended { 20 } else { 8 });
                        frame.insert(at, unavailable);
                    }
                    // Cut short.
                    _ => {
                        let keep = random(frame[at].len());
                        frame[at].truncate(keep);
                    }
                }
            }
            packets.extend(frame);
        }
        let payload = (width as usize + 2) * height as usize + 3;
        let mut whole = Assembler::new(payload + random(3) * block_size, block_size + 36, 0);
        whole.limits = [Some((block_size, payload.div_ceil(block_size) + 4)); 2];
        whole.resend = round.is_multiple_of(2);
        (packets, whole)
    }
    /// A second assembler set up as `like` is.
    fn alike(like: &Assembler) -> Assembler {
        let mut assembler = Assembler::new(like.payload_limit, like.limits[0].unwrap().0 + 36, 0);
        assembler.limits = like.limits;
        assembler.resend = like.resend;
        assembler
    }

    /// Generated packet sequences give the same frames, errors, counts and
    /// state received in place as received whole.
    #[test]
    fn packets_received_in_place_assemble_as_packets_received_whole() {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut random = move |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            usize::try_from(seed % n.max(1) as u64).unwrap()
        };
        let mut landed_total = 0;
        for round in 0..400 {
            let (packets, mut whole) = generated(&mut random, round);
            let mut placed = alike(&whole);
            assert_eq!(
                outcomes(&mut placed, &packets, true),
                outcomes(&mut whole, &packets, false),
                "round {round}"
            );
            landed_total += whole.copied.0 - placed.copied.0;
        }
        // Most payload packets landed in place.
        assert!(landed_total > 2000, "{landed_total}");
    }

    /// The frames `packets` complete, taken in by `assembler` as batches of
    /// up to `batch::BATCH` datagrams of sizes `random` picks: each first
    /// written where the kernel would put it (the predicted ones into their
    /// header, slot and rest), then all taken in together. Without
    /// `batches`, one at a time and whole.
    fn assembled(
        assembler: &mut Assembler,
        packets: &[Vec<u8>],
        mut batches: Option<&mut dyn FnMut(usize) -> usize>,
    ) -> Vec<(u64, Vec<u8>)> {
        let now = Instant::now();
        let mut frames = Vec::new();
        let mut keep = |result: Result<Option<Frame>>, assembler: &mut Assembler| {
            for frame in result
                .ok()
                .flatten()
                .into_iter()
                .chain(assembler.ready.drain(..))
            {
                frames.push((frame.id, frame.data.to_vec()));
            }
        };
        let Some(random) = batches.as_mut() else {
            for packet in packets {
                let result = assembler.push_counted(packet, now, Site::GvspAssemble);
                keep(result, assembler);
            }
            return frames;
        };
        let mut batch = batch::Batch::new(64 * 1024);
        let mut at = 0;
        while at < packets.len() {
            let count = (1 + random(batch::BATCH)).min(packets.len() - at);
            let slots: Vec<_> = assembler.predict_run(count).collect();
            let base = if slots.is_empty() {
                std::ptr::null_mut()
            } else {
                assembler.newest_buffer().unwrap()
            };
            let mut received = Vec::new();
            for (index, packet) in packets[at..at + count].iter().enumerate() {
                match slots.get(index) {
                    Some(slot) => {
                        let head = packet.len().min(slot.header);
                        batch.headers[index] = [0xEE; 20];
                        batch.headers[index][..head].copy_from_slice(&packet[..head]);
                        let rest = &packet[head..];
                        let in_slot = rest.len().min(slot.len);
                        assembler.slot_mut(slot).unwrap()[..in_slot]
                            .copy_from_slice(&rest[..in_slot]);
                        batch.rests[index][..rest.len() - in_slot]
                            .copy_from_slice(&rest[in_slot..]);
                    }
                    None => batch.rests[index][..packet.len()].copy_from_slice(packet),
                }
                received.push(batch::Received {
                    len: packet.len(),
                    truncated: false,
                    from_camera: true,
                });
            }
            if let Some(result) = batch.take(
                assembler,
                (&slots, base),
                &received,
                Site::GvspFallback,
                now,
            ) {
                keep(result, assembler);
            }
            at += count;
        }
        frames
    }

    /// Generated packet sequences received in batches, each payload packet
    /// in its predicted place, give the frames they give received whole,
    /// though one that lands on a later one's place is put in its own.
    #[test]
    fn packets_received_in_batches_assemble_as_packets_received_whole() {
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut random = move |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            usize::try_from(seed % n.max(1) as u64).unwrap()
        };
        let mut sizes = {
            let mut seed = 0x9e37_79b9u64;
            move |n: usize| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                usize::try_from(seed % n.max(1) as u64).unwrap()
            }
        };
        let (mut landed, mut delivered) = (0, 0);
        for round in 0..400 {
            let (packets, mut whole) = generated(&mut random, round);
            let mut batched = alike(&whole);
            let frames = assembled(&mut whole, &packets, None);
            assert_eq!(
                assembled(&mut batched, &packets, Some(&mut sizes)),
                frames,
                "round {round}"
            );
            assert_eq!(
                (batched.stats.incomplete_frames, batched.stats.lost_frames),
                (whole.stats.incomplete_frames, whole.stats.lost_frames),
                "round {round}"
            );
            landed += whole.copied.0 - batched.copied.0;
            delivered += frames.len();
        }
        // Packets of a frame whose leader came within the batch are whole,
        // which for these frames of a few packets each is most of them.
        assert!(landed > 500 && delivered > 300, "{landed} {delivered}");
    }

    /// A fake camera on loopback streaming `frames` 1080p Mono8 frames in
    /// packets of `block` bytes, sent in `order`, received a datagram at a
    /// time or, `batched`, several (Linux): what each frame held, and the
    /// payload packets and bytes copied.
    fn loopback(
        frames: u64,
        batched: bool,
        order: impl Fn(&mut Vec<Vec<u8>>),
    ) -> (Vec<Vec<u8>>, (u64, u64)) {
        const BLOCK: usize = 8192;
        let fake = Fake::new(|camera| {
            camera.path_mtu = 9000;
            camera.test_packets = true;
            camera.registers.insert(GVCP_CAPABILITY, 0x4);
            camera.resend_status = 0x0100;
        });
        let mut gige = connect(fake.address, Duration::from_secs(1)).unwrap();
        gige.packet_size = PacketSize::Verified(u32::try_from(BLOCK).unwrap() + 36);
        gige.start(1920 * 1080).unwrap();
        assert!(gige.stream.as_ref().unwrap().scatter);
        assert_eq!(
            gige.stream.as_ref().unwrap().batch.is_some(),
            cfg!(target_os = "linux")
        );
        if !batched {
            gige.stream.as_mut().unwrap().batch = None;
        }
        let mut received = Vec::new();
        for block in 1..=frames {
            let mut packets = Vec::new();
            let mut destination = None;
            fake.send_sized(block, false, (1920, 1080), &[], |packet, to| {
                packets.push(packet.to_vec());
                destination = Some(to);
            });
            order(&mut packets);
            // Send while receiving, with no assumption about the VM's CPU
            // share. A fixed sleep can still overflow a capped receive queue
            // when the receiver is descheduled beside other large frame tests.
            let (source, destination) = (fake.stream.try_clone().unwrap(), destination.unwrap());
            #[cfg(target_os = "linux")]
            let (receiver, burst) = {
                let socket = &gige.stream.as_ref().unwrap().socket;
                let capacity = socket2::SockRef::from(socket).recv_buffer_size().unwrap();
                // Linux includes bookkeeping in SO_RCVBUF; leave another
                // factor of two for skb overhead and any requested resends.
                (
                    socket.try_clone().unwrap(),
                    (capacity / (4 * BLOCK)).clamp(1, 8),
                )
            };
            #[cfg(not(target_os = "linux"))]
            let burst = 8;
            let sender = thread::spawn(move || {
                #[cfg(target_os = "linux")]
                let deadline = Instant::now() + Duration::from_secs(5);
                for (i, packet) in packets.iter().enumerate() {
                    source.send_to(packet, destination).unwrap();
                    // Do not wait after the final burst: next_frame can return
                    // while a late duplicate/resend is still queued, and its
                    // caller joins this sender before beginning another read.
                    if (i + 1).is_multiple_of(burst) && i + 1 < packets.len() {
                        #[cfg(target_os = "linux")]
                        {
                            use std::os::fd::AsRawFd as _;
                            loop {
                                assert!(
                                    Instant::now() < deadline,
                                    "loopback receiver did not drain its socket queue"
                                );
                                let mut byte = [0u8];
                                // MSG_DONTWAIT applies to this call only:
                                // set_nonblocking would also alter the cloned
                                // receiver descriptor used by next_frame.
                                // SAFETY: receiver stays live and recv can write
                                // at most the one byte in this valid allocation.
                                let queued = unsafe {
                                    libc::recv(
                                        receiver.as_raw_fd(),
                                        byte.as_mut_ptr().cast(),
                                        byte.len(),
                                        libc::MSG_PEEK | libc::MSG_DONTWAIT,
                                    )
                                };
                                if queued < 0 {
                                    let error = std::io::Error::last_os_error();
                                    if error.kind() == ErrorKind::WouldBlock {
                                        break;
                                    }
                                    assert_eq!(error.kind(), ErrorKind::Interrupted, "{error}");
                                    continue;
                                }
                                thread::sleep(Duration::from_micros(100));
                            }
                        }
                        #[cfg(not(target_os = "linux"))]
                        thread::sleep(Duration::from_micros(300));
                    }
                }
            });
            let frame = gige.next_frame(Duration::from_secs(5)).unwrap();
            sender.join().unwrap();
            assert_eq!(frame.id, block);
            received.push(frame.data.to_vec());
        }
        let copied = gige.stream.as_ref().unwrap().assembler.copied;
        (received, copied)
    }

    #[test]
    fn packets_in_order_land_in_their_frames_without_copies() {
        for batched in [false, true] {
            let (frames, copied) = loopback(30, batched, |_| {});
            assert!(frames.iter().all(|frame| *frame == image(1920, 1080)));
            assert_eq!(copied, (0, 0), "batched {batched}");
        }
    }

    #[test]
    fn reordered_and_resent_packets_are_copied_one_by_one() {
        // Two payload packets swap places: each arrives where the other was
        // expected.
        let (frames, copied) = loopback(3, false, |packets| packets.swap(5, 6));
        assert!(frames.iter().all(|frame| *frame == image(1920, 1080)));
        assert_eq!(copied, (6, 6 * 8192));
        // A lost packet is asked for again and comes in out of turn; the
        // packet after it arrived where the lost one was expected.
        let lose = |packets: &mut Vec<Vec<u8>>| {
            packets.remove(40);
            packets.remove(9);
        };
        let (frames, copied) = loopback(2, false, lose);
        assert!(frames.iter().all(|frame| *frame == image(1920, 1080)));
        assert_eq!(copied, (8, 8 * 8192));
        // In batches, every packet of the batch after a lost one arrived a
        // place early and is copied, but no more.
        let (frames, copied) = loopback(2, true, lose);
        assert!(frames.iter().all(|frame| *frame == image(1920, 1080)));
        assert!(copied.0 <= 2 * 2 * batch::BATCH as u64, "{copied:?}");
        let (frames, _) = loopback(3, true, |packets| packets.swap(5, 6));
        assert!(frames.iter().all(|frame| *frame == image(1920, 1080)));
    }

    /// A short packet that arrives where a payload packet was expected
    /// leaves the next frames' leaders and trailers whole.
    #[test]
    fn frames_after_a_short_mispredicted_packet_arrive() {
        // The last payload packet is lost: the trailer arrives in its place
        // and the resent packet completes the frame.
        for batched in [false, true] {
            let (frames, copied) = loopback(3, batched, |packets| {
                let last = packets.len() - 2;
                packets.remove(last);
            });
            assert!(frames.iter().all(|frame| *frame == image(1920, 1080)));
            assert_eq!(copied, (0, 0), "batched {batched}");
        }
        // A header-only "packet temporarily unavailable" resend reply where
        // the packet itself was expected, which arrives next. In batches, the
        // packets after it in the batch arrived a place late and are copied.
        let unavailable = |packets: &mut Vec<Vec<u8>>| {
            let mut unavailable = packets[9][..8].to_vec();
            unavailable[..2].copy_from_slice(&0x8014u16.to_be_bytes());
            packets.insert(9, unavailable);
        };
        let (frames, copied) = loopback(3, false, unavailable);
        assert!(frames.iter().all(|frame| *frame == image(1920, 1080)));
        assert_eq!(copied, (0, 0));
        let (frames, copied) = loopback(3, true, unavailable);
        assert!(frames.iter().all(|frame| *frame == image(1920, 1080)));
        assert!(copied.0 <= 3 * batch::BATCH as u64, "{copied:?}");
    }

    #[test]
    fn scatter_receive_can_be_turned_off() {
        assert!(scatter(None));
        assert!(scatter(Some(std::ffi::OsStr::new("1"))));
        assert!(!scatter(Some(std::ffi::OsStr::new("0"))));
        let fake = Fake::new(|camera| camera.path_mtu = 1500);
        let mut gige = connect(fake.address, Duration::from_secs(1)).unwrap();
        gige.packet_size = PacketSize::Verified(1500);
        gige.start((WIDTH * HEIGHT) as usize).unwrap();
        gige.stream.as_mut().unwrap().scatter = false;
        fake.send(1, false, &[]);
        let frame = gige.next_frame(Duration::from_secs(2)).unwrap();
        assert_eq!(frame.data.to_vec(), image(WIDTH, HEIGHT));
        let packets = u64::from(WIDTH * HEIGHT) / (1500 - 36) + 1;
        assert_eq!(
            gige.stream.as_ref().unwrap().assembler.copied,
            (packets, u64::from(WIDTH * HEIGHT))
        );
    }

    /// The receiving thread's CPU time.
    fn thread_cpu() -> Duration {
        #[cfg(unix)]
        {
            let mut time = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            // SAFETY: clock_gettime writes the timespec it is given.
            unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &raw mut time) };
            Duration::new(
                u64::try_from(time.tv_sec).unwrap(),
                u32::try_from(time.tv_nsec).unwrap(),
            )
        }
        #[cfg(not(unix))]
        Duration::ZERO
    }

    /// Frames per second and receiver CPU time per frame for 4096x3000
    /// Mono8 over loopback, with payloads received in place and copied,
    /// in batches (Linux) and a datagram at a time.
    #[test]
    #[ignore = "a throughput measurement"]
    fn scatter_receive_throughput() {
        const BLOCK: usize = 8192;
        const FRAMES: u64 = 40;
        let batches: &[bool] = if cfg!(target_os = "linux") {
            &[true, false]
        } else {
            &[false]
        };
        for (scatter, batched) in [true, false]
            .into_iter()
            .flat_map(|scatter| batches.iter().map(move |&batched| (scatter, batched)))
        {
            let fake = Fake::new(|camera| {
                camera.path_mtu = 9000;
                camera.test_packets = true;
            });
            let mut gige = connect(fake.address, Duration::from_secs(1)).unwrap();
            gige.packet_size = PacketSize::Verified(u32::try_from(BLOCK).unwrap() + 36);
            gige.start(4096 * 3000).unwrap();
            gige.stream.as_mut().unwrap().scatter = scatter;
            if !batched {
                gige.stream.as_mut().unwrap().batch = None;
            }
            let mut packets = Vec::new();
            let mut destination = None;
            fake.send_sized(1, false, (4096, 3000), &[], |packet, to| {
                packets.push(packet.to_vec());
                destination = Some(to);
            });
            let (destination, source) = (destination.unwrap(), fake.stream.try_clone().unwrap());
            // The sender waits for each frame before the next, and pauses
            // every 4 packets, so a small socket buffer (Linux's default,
            // about 200 KiB) never overflows; frame rates are then the
            // sender's, and the receiver's CPU time is what is compared.
            let (next, wait) = std::sync::mpsc::channel::<u64>();
            let sender = thread::spawn(move || {
                while let Ok(block) = wait.recv() {
                    for (i, packet) in packets.iter().enumerate() {
                        let mut packet = packet.clone();
                        packet[2..4].copy_from_slice(&block.to_be_bytes()[6..]);
                        source.send_to(&packet, destination).unwrap();
                        if i % 4 == 3 {
                            thread::sleep(Duration::from_micros(100));
                        }
                    }
                }
            });
            let (started, cpu) = (Instant::now(), thread_cpu());
            let mut received = 0;
            for block in 1..=FRAMES {
                next.send(block).unwrap();
                // A receiver that falls behind loses a frame to the full
                // socket buffer; this camera resends nothing.
                if let Ok(frame) = gige.next_frame(Duration::from_secs(1)) {
                    assert_eq!(frame.data.len(), 4096 * 3000);
                    received += 1;
                }
            }
            let (elapsed, cpu) = (
                started.elapsed(),
                thread_cpu()
                    .checked_sub(cpu)
                    .expect("thread CPU clock is monotonic"),
            );
            drop(next);
            sender.join().unwrap();
            assert!(received > 0);
            println!(
                "scatter {scatter}, batches {batched}: {received} of {FRAMES} frames, {:.1} fps, receiver CPU {:.2} ms per frame, copied {:?}",
                f64::from(received) / elapsed.as_secs_f64(),
                cpu.as_secs_f64() * 1e3 / f64::from(received),
                gige.stream.as_ref().unwrap().assembler.copied,
            );
        }
    }
    fn controlled_clock(gige: &mut GigE) -> Arc<std::sync::atomic::AtomicU64> {
        gige.heartbeat.shutdown();
        let timeout = gige.heartbeat.lease.timeout_ms().unwrap();
        let disabled = gige.heartbeat.lease.disabled();
        let (lease, now) = Lease::testing(timeout, disabled);
        gige.control.lock().unwrap().lease = Some(lease.clone());
        gige.heartbeat.lease = lease.clone();
        if let Some(stream) = &mut gige.stream {
            stream.lease = Some(lease);
        }
        now
    }
    #[test]
    fn expired_controller_never_mutates_or_resends_to_a_replacement_owner() {
        let (fake, mut gige) = streaming(|camera| {
            camera.enforce_owner = true;
            camera.registers.insert(GVCP_CAPABILITY, 0x4);
        });
        fake.send(901, true, &[]);
        let captured = gige.next_frame(Duration::from_secs(2)).unwrap();
        let original = captured.data.as_ref().to_vec();
        let now = controlled_clock(&mut gige);
        let replacement = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 62222);
        {
            let mut camera = fake.camera.lock().unwrap();
            camera.owner = Some(replacement);
            camera.registers.insert(CONTROL_PRIVILEGE, 2);
            camera.registers.insert(STREAM_PORT, 32123);
            camera.registers.insert(GVCP_CONFIGURATION, 0x40);
            camera.registers.insert(HEARTBEAT_TIMEOUT, 1234);
            camera.commands.clear();
            camera.register_writes.clear();
        }
        now.store(4_000_000_000, Ordering::SeqCst);
        let error = gige.next_frame(Duration::from_millis(10)).unwrap_err();
        assert!(ownership_retired(&error));
        assert_eq!(
            crate::types::classify(&error).kind,
            crate::types::ErrorKind::Disconnected
        );
        assert!(ownership_retired(&gige.read_memory(0x100, 4).unwrap_err()));
        assert!(ownership_retired(
            &gige.write_memory(0x100, &[1, 2, 3, 4]).unwrap_err()
        ));
        assert!(ownership_retired(&gige.start(8000).unwrap_err()));
        assert!(ownership_retired(&gige.optimize_network(8000).unwrap_err()));
        assert_eq!(gige.device_time_ns(), None);
        assert_eq!(gige.buffered_frames(), 0);
        let stream = gige.stream.as_mut().unwrap();
        stream.assembler.requests.push((902, true, 1, 2));
        stream.flush();
        assert!(ownership_retired(&gige.stop().unwrap_err()));
        drop(gige);
        let camera = fake.camera.lock().unwrap();
        assert!(
            camera.commands.is_empty(),
            "stale commands: {:?}",
            camera.commands
        );
        assert_eq!(camera.register_writes, Vec::new());
        assert_eq!(camera.owner, Some(replacement));
        assert_eq!(camera.register(STREAM_PORT), 32123);
        assert_eq!(camera.register(GVCP_CONFIGURATION), 0x40);
        assert_eq!(camera.register(HEARTBEAT_TIMEOUT), 1234);
        assert_eq!(captured.data.as_ref(), original);
    }
    #[test]
    fn advertised_owner_endpoint_is_full_tuple_and_never_a_fallback() {
        for mismatch in ["port", "ip", "zero", "ccp_zero", "switchover", "unreadable"] {
            let fake = Fake::new(|camera| {
                camera.enforce_owner = true;
                camera.registers.insert(GVCP_CAPABILITY, OWNER_ENDPOINT);
            });
            let mut gige = connect(fake.address, Duration::from_millis(100)).unwrap();
            let now = controlled_clock(&mut gige);
            {
                let mut camera = fake.camera.lock().unwrap();
                match mismatch {
                    "port" => {
                        let port = camera.register(PRIMARY_PORT);
                        camera.registers.insert(PRIMARY_PORT, port + 1);
                    }
                    "ip" => {
                        camera
                            .registers
                            .insert(PRIMARY_IP, u32::from(Ipv4Addr::new(127, 0, 0, 2)));
                    }
                    "zero" => {
                        camera.registers.insert(PRIMARY_PORT, 0);
                        camera.registers.insert(PRIMARY_IP, 0);
                    }
                    "ccp_zero" => {
                        camera.registers.insert(CONTROL_PRIVILEGE, 0);
                    }
                    "switchover" => {
                        camera.registers.insert(CONTROL_PRIVILEGE, 6);
                    }
                    _ => {
                        camera.unreadable.push(PRIMARY_PORT);
                    }
                }
                camera.commands.clear();
            }
            let error = gige.control.lock().unwrap().probe_owner().unwrap_err();
            if mismatch == "unreadable" {
                assert_eq!(error.downcast_ref::<GvcpStatus>().unwrap().0, 0x8003);
            } else {
                assert!(ownership_retired(&error));
            }
            // A missing advertised field never renews the old finite proof.
            if mismatch == "unreadable" {
                now.store(4_000_000_000, Ordering::SeqCst);
            }
            assert!(ownership_retired(&gige.heartbeat.check().unwrap_err()));
            fake.camera
                .lock()
                .unwrap()
                .registers
                .insert(CONTROL_PRIVILEGE, 2);
            assert!(ownership_retired(&gige.heartbeat.check().unwrap_err()));
            let before = fake.camera.lock().unwrap().commands.len();
            drop(gige);
            assert_eq!(fake.camera.lock().unwrap().commands.len(), before);
        }
    }
    #[test]
    fn late_initial_claim_never_grants_disabled_or_releases_an_unknown_owner() {
        for (disabled, late_readback) in
            [(false, false), (true, false), (false, true), (true, true)]
        {
            let now = Arc::new(std::sync::atomic::AtomicU64::new(1_000_000_000));
            let fake = Fake::new(|camera| {
                camera.enforce_owner = true;
                camera.registers.insert(HEARTBEAT_TIMEOUT, 500);
                camera.registers.insert(GVCP_CAPABILITY, DISABLE_CAPABILITY);
                camera
                    .registers
                    .insert(GVCP_CONFIGURATION, u32::from(disabled));
                if late_readback {
                    camera.jump_on_read = Some((
                        if disabled {
                            GVCP_CONFIGURATION
                        } else {
                            HEARTBEAT_TIMEOUT
                        },
                        now.clone(),
                        1_500_000_000,
                    ));
                } else {
                    camera.jump_on_claim = Some((now.clone(), 1_500_000_000));
                }
            });
            let mut control = Control::connect(fake.address, Duration::from_millis(100)).unwrap();
            control.lease_clock = Some(now);
            let Err(error) = connect_control(control) else {
                panic!("late claim was accepted");
            };
            assert!(ownership_retired(&error), "{error:#}");
            let camera = fake.camera.lock().unwrap();
            assert_eq!(camera.register_writes, [(CONTROL_PRIVILEGE, 2)]);
            assert_eq!(
                camera
                    .commands
                    .iter()
                    .filter(|(command, _)| *command == WRITE_REGISTER)
                    .count(),
                1
            );
        }
    }
    #[test]
    fn late_ccp_reply_and_pending_ack_cannot_renew_the_old_proof() {
        let fake = Fake::new(|camera| {
            camera.enforce_owner = true;
        });
        let mut gige = connect(fake.address, Duration::from_millis(100)).unwrap();
        let now = controlled_clock(&mut gige);
        fake.camera.lock().unwrap().jump_on_read = Some((CONTROL_PRIVILEGE, now, 4_000_000_000));
        let error = gige.control.lock().unwrap().probe_owner().unwrap_err();
        assert!(ownership_retired(&error));
        let reads = fake.camera.lock().unwrap().commands.len();
        drop(gige);
        assert_eq!(fake.camera.lock().unwrap().commands.len(), reads);
        drop(fake);

        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_millis(80)))
            .unwrap();
        let SocketAddr::V4(address) = socket.local_addr().unwrap() else {
            unreachable!()
        };
        let (lease, now) = Lease::testing(2000, false);
        let server = thread::spawn(move || {
            let mut request = [0u8; 128];
            let (_, peer) = socket.recv_from(&mut request).unwrap();
            let id = be16(&request, 6);
            let pending = ack(PENDING_ACK, id, 0, &[0, 0, 3, 232]);
            now.store(3_000_000_000, Ordering::SeqCst);
            socket.send_to(&pending, peer).unwrap();
            socket
                .send_to(&ack(READ_REGISTER + 1, id, 0, &2u32.to_be_bytes()), peer)
                .unwrap();
            let error = socket.recv_from(&mut request).unwrap_err();
            assert!(is_receive_timeout(&error));
        });
        let mut control = Control::connect(address, Duration::from_millis(100)).unwrap();
        control.lease = Some(lease);
        assert!(ownership_retired(
            &control.read_register(0x100).unwrap_err()
        ));
        assert!(ownership_retired(
            &control.write_register(0x100, 1).unwrap_err()
        ));
        control.release(2000);
        server.join().unwrap();
    }
    #[test]
    fn full_command_budget_refreshes_before_a_clipped_long_call() {
        let fake = Fake::new(|camera| {
            camera.registers.insert(CONTROL_PRIVILEGE, 2);
            camera.registers.insert(0x100, 42);
        });
        let mut control = Control::connect(fake.address, Duration::from_millis(900)).unwrap();
        let (lease, now) = Lease::testing(1000, false);
        now.store(1_300_000_000, Ordering::SeqCst);
        control.lease = Some(lease.clone());
        assert_eq!(control.read_register(0x100).unwrap(), 42);
        assert_eq!(
            fake.camera.lock().unwrap().register_reads,
            [CONTROL_PRIVILEGE, 0x100]
        );
        assert_eq!(
            lease.remaining().unwrap(),
            Some(Duration::from_millis(1000))
        );
    }
    #[test]
    fn finite_timer_and_partial_or_mixed_managed_writes_fail_before_wire() {
        let fake = Fake::new(|camera| {
            camera.enforce_owner = true;
        });
        let mut gige = connect(fake.address, Duration::from_millis(100)).unwrap();
        let _ = controlled_clock(&mut gige);
        let mut control = gige.control.lock().unwrap();
        fake.camera.lock().unwrap().commands.clear();
        for value in [0, 500, 4000] {
            let error = control
                .write_register(HEARTBEAT_TIMEOUT, value)
                .unwrap_err();
            assert_eq!(
                crate::types::classify(&error).kind,
                crate::types::ErrorKind::Unsupported
            );
        }
        for value in [1, 3, 6, 0x1234_0002] {
            let error = control
                .write_register(CONTROL_PRIVILEGE, value)
                .unwrap_err();
            assert_eq!(
                crate::types::classify(&error).kind,
                crate::types::ErrorKind::Unsupported
            );
        }
        for (address, data) in [
            (u64::from(HEARTBEAT_TIMEOUT), vec![1]),
            (u64::from(HEARTBEAT_TIMEOUT) - 4, vec![0; 8]),
            (u64::from(CONTROL_PRIVILEGE) + 1, vec![0; 3]),
        ] {
            let error = control.write_bytes(address, &data).unwrap_err();
            assert_eq!(
                crate::types::classify(&error).kind,
                crate::types::ErrorKind::Unsupported
            );
        }
        let error = control.write_register(SWITCHOVER_KEY, 12).unwrap_err();
        assert_eq!(
            crate::types::classify(&error).kind,
            crate::types::ErrorKind::Unsupported
        );
        assert_eq!(fake.camera.lock().unwrap().commands, Vec::new());
        drop(control);
    }
    #[test]
    fn managed_whole_memory_words_follow_verified_device_order() {
        for (legacy, little) in [(true, false), (true, true), (false, false), (false, true)] {
            let fake = Fake::new(|camera| {
                camera.enforce_owner = true;
                camera.little_endian = little;
                camera
                    .registers
                    .insert(DEVICE_MODE, if little { 0 } else { 0x8000_0000 });
            });
            let mut gige = connect(fake.address, Duration::from_millis(100)).unwrap();
            let now = controlled_clock(&mut gige);
            let mut control = gige.control.lock().unwrap();
            control.legacy_registers = legacy;
            fake.camera.lock().unwrap().register_writes.clear();
            let bytes = if !legacy && little {
                3000u32.to_le_bytes()
            } else {
                3000u32.to_be_bytes()
            };
            now.store(1_500_000_000, Ordering::SeqCst);
            control
                .write_bytes(u64::from(HEARTBEAT_TIMEOUT), &bytes)
                .unwrap();
            assert_eq!(
                control.lease.as_ref().unwrap().remaining().unwrap(),
                Some(Duration::from_millis(2500))
            );
            let camera = fake.camera.lock().unwrap();
            assert_eq!(camera.register_writes, [(HEARTBEAT_TIMEOUT, 3000)]);
            assert_eq!(camera.memory_writes, Vec::new());
            drop(camera);
            drop(control);
        }
    }
    #[test]
    fn disabled_wire_transition_retains_shadow_and_owned_release() {
        let fake = Fake::new(|camera| {
            camera.enforce_owner = true;
            camera
                .registers
                .insert(GVCP_CAPABILITY, DISABLE_CAPABILITY | OWNER_ENDPOINT);
        });
        let mut gige = connect(fake.address, Duration::from_millis(100)).unwrap();
        let now = controlled_clock(&mut gige);
        let mut control = gige.control.lock().unwrap();
        control
            .write_register(GVCP_CONFIGURATION, HEARTBEAT_DISABLE)
            .unwrap();
        assert!(control.lease.as_ref().unwrap().disabled());
        now.store(1_100_000_000, Ordering::SeqCst);
        control.write_register(GVCP_CONFIGURATION, 0).unwrap();
        assert!(!control.lease.as_ref().unwrap().disabled());
        control
            .write_register(GVCP_CONFIGURATION, HEARTBEAT_DISABLE)
            .unwrap();
        now.store(5_000_000_000, Ordering::SeqCst);
        control.probe_owner().unwrap();
        let before = fake.camera.lock().unwrap().register_writes.len();
        let error = control.write_register(GVCP_CONFIGURATION, 0).unwrap_err();
        assert_eq!(
            crate::types::classify(&error).kind,
            crate::types::ErrorKind::Unsupported
        );
        assert_eq!(fake.camera.lock().unwrap().register_writes.len(), before);
        // A timeout read error removes finite permission, while the verified
        // disabled owner can still deliberately release once.
        fake.camera
            .lock()
            .unwrap()
            .unreadable
            .push(HEARTBEAT_TIMEOUT);
        let error = control.write_register(HEARTBEAT_TIMEOUT, 500).unwrap_err();
        assert_eq!(error.downcast_ref::<GvcpStatus>().unwrap().0, 0x8003);
        assert!(control.lease.as_ref().unwrap().disabled());
        assert_eq!(control.lease.as_ref().unwrap().timeout_ms(), None);
        control.write_register(CONTROL_PRIVILEGE, 0).unwrap();
        assert_eq!(fake.register(CONTROL_PRIVILEGE), 0);
        drop(control);
    }
    #[test]
    fn raw_release_restores_only_owned_bits_before_one_lost_ack_and_new_owner() {
        let fake = Fake::new(|camera| {
            camera.enforce_owner = true;
            camera
                .registers
                .insert(GVCP_CAPABILITY, DISABLE_CAPABILITY | OWNER_ENDPOINT | 0x20);
            camera
                .registers
                .insert(GVCP_CONFIGURATION, HEARTBEAT_DISABLE);
        });
        let mut gige = connect(fake.address, Duration::from_millis(90)).unwrap();
        let _ = controlled_clock(&mut gige);
        let replacement = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 62223);
        {
            let mut camera = fake.camera.lock().unwrap();
            camera.registers.insert(GVCP_CONFIGURATION, 0x43);
            camera.release_replacement = Some(replacement);
            camera.commands.clear();
            camera.register_writes.clear();
        }
        let mut control = gige.control.lock().unwrap();
        let error = control.write_register(CONTROL_PRIVILEGE, 0).unwrap_err();
        assert!(error.downcast_ref::<GvcpTimeout>().is_some());
        assert!(ownership_retired(
            &control.read_register(CONTROL_PRIVILEGE).unwrap_err()
        ));
        drop(control);
        drop(gige);
        let camera = fake.camera.lock().unwrap();
        assert_eq!(
            camera.register_writes,
            [(GVCP_CONFIGURATION, 0x41), (CONTROL_PRIVILEGE, 0)]
        );
        assert_eq!(camera.commands.last().unwrap().0, WRITE_REGISTER);
        assert_eq!(
            camera
                .commands
                .iter()
                .filter(|(command, _)| *command == WRITE_REGISTER)
                .count(),
            2
        );
        assert_eq!(camera.owner, Some(replacement));
        assert_eq!(camera.register(GVCP_CONFIGURATION), 0x41);
        assert_eq!(camera.register(CONTROL_PRIVILEGE), 2);
    }
    #[test]
    fn lost_claim_ack_never_reclaims_or_cleans_up_a_hidden_new_owner() {
        let now = Arc::new(std::sync::atomic::AtomicU64::new(1_000_000_000));
        let replacement = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 62224);
        let fake = Fake::new(|camera| {
            camera.enforce_owner = true;
            camera.drop_claim_ack = true;
            camera.claim_replacement = Some(replacement);
            camera.jump_on_claim = Some((now.clone(), 1_200_000_000));
        });
        let mut control = Control::connect(fake.address, Duration::from_millis(90)).unwrap();
        control.lease_clock = Some(now);
        let Err(error) = connect_control(control) else {
            panic!("unconfirmed claim was accepted");
        };
        assert!(error.downcast_ref::<GvcpTimeout>().is_some(), "{error:#}");
        let camera = fake.camera.lock().unwrap();
        assert_eq!(camera.register(HEARTBEAT_TIMEOUT), 3000);
        assert_eq!(camera.owner, Some(replacement));
        assert_eq!(camera.register(CONTROL_PRIVILEGE), 2);
        assert_eq!(camera.register_writes, [(CONTROL_PRIVILEGE, 2)]);
        assert_eq!(
            camera
                .commands
                .iter()
                .filter(|(command, _)| *command == WRITE_REGISTER)
                .count(),
            1
        );
    }
    #[test]
    fn ids_custom_codes_do_not_gain_native_gvsp_wire_semantics() {
        for code in [
            0x4000_000f_u32,
            0x4000_001f,
            0x4000_0001,
            0x4000_0011,
            0x4000_0002,
            0x4000_0012,
            0x4000_0003,
            0x4000_0013,
            0x4000_0004,
            0x4000_0014,
        ] {
            for payload in [1_u16, 5, 0x4001] {
                let mut bytes = leader(4, 2, code);
                bytes[2..4].copy_from_slice(&payload.to_be_bytes());
                assert!(
                    ImageLeader::parse(&bytes, 128)
                        .unwrap_err()
                        .to_string()
                        .contains("pixel format")
                );
            }
            let body = [leader(4, 2, code), trailer(2), vec![0; 12]].concat();
            let packet = gvsp(42, 0x00ff_ffff, 4, false, &body);
            let mut assembler = Assembler::new(128, 1500, 0);
            assert!(
                assembler
                    .push(&packet)
                    .unwrap_err()
                    .to_string()
                    .contains("pixel format")
            );
            assert!(assembler.frames.is_empty() && assembler.ready.is_empty());
            assert_eq!(assembler.copied, (0, 0));
            let frame = frame_packets(43, false, 2, 2, 1464)
                .iter()
                .find_map(|packet| assembler.push(packet).unwrap())
                .unwrap();
            assert_eq!(frame.data, image(2, 2));
            assert!(frame.pixel_encoding.is_none());
        }
    }

    #[test]
    fn packed_bicolor_reordered_packets_and_resend_keep_exact_native_bytes() {
        let literal: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/bicolor/packed-literals.json"
        ))
        .unwrap();
        for (case, codes) in [
            (0, [0x0214_00a8, 0x0214_00aa]),
            (1, [0x0218_00ac, 0x0218_00ae]),
        ] {
            let case = &literal["cases"][case];
            let raw = crate::types::decode_hex(case["tight_hex"].as_str().unwrap(), 128).unwrap();
            let width = u32::try_from(case["width"].as_u64().unwrap()).unwrap();
            assert!(raw.len() > 16);
            for code in codes {
                for big in [false, true] {
                    assert_eq!(sample_bytes(code), None);
                    let start = Instant::now();
                    let mut assembler = Assembler::new(raw.len(), 44, 0);
                    assembler.big_endian = big;
                    assembler.resend = true;
                    let packets = [
                        gvsp(1, 0, 1, false, &leader(width, 2, code)),
                        gvsp(1, 1, 3, false, &raw[..8]),
                        gvsp(1, 2, 3, false, &raw[8..16]),
                        gvsp(1, 3, 3, false, &raw[16..]),
                        gvsp(1, 4, 2, false, &trailer(2)),
                    ];
                    for index in [0, 3, 1, 4] {
                        assert!(assembler.push_at(&packets[index], start).unwrap().is_none());
                    }
                    assembler.poll(start + RESEND_IDLE);
                    assert!(assembler.ready.is_empty());
                    assert!(
                        assembler
                            .requests
                            .iter()
                            .any(|&(block, extended, first, last)| block == 1
                                && !extended
                                && first <= 2
                                && last >= 2)
                    );
                    let recovered = assembler.stats.resend_recovered;
                    let mut resent = packets[2].clone();
                    resent[..2].copy_from_slice(&0x0100_u16.to_be_bytes());
                    let frame = assembler
                        .push_at(&resent, start + RESEND_IDLE)
                        .unwrap()
                        .unwrap();
                    assert_eq!(
                        (frame.width, frame.height, frame.pixel_format),
                        (width, 2, code)
                    );
                    assert_eq!(frame.data.as_ref(), raw);
                    crate::frame::validate_source(&frame).unwrap();
                    assert_eq!(assembler.stats.resend_recovered, recovered + 1);
                    assert!(assembler.frames.is_empty());
                }
            }
        }
        for (width, height) in [(1, 1), (3, 1), (3, 2)] {
            for code in [0x0214_00a8, 0x0214_00aa] {
                let error = ImageLeader::parse(&leader(width, height, code), 64).unwrap_err();
                assert_eq!(
                    crate::types::classify(&error).kind,
                    crate::types::ErrorKind::Unsupported
                );
            }
        }
    }
}
