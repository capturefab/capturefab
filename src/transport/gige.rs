//! A native Rust GigE Vision control and image transport.
//!
//! Wire constants and layouts follow the GigE Vision bootstrap/GVCP/GVSP
//! protocols (also documented in Aravis' protocol headers). This implementation
//! does not link to, embed, or translate Aravis code.
use crate::types::{Backend, CameraInfo, Frame, RegisterIo, Transport, TransportStats};
use anyhow::{Context, Result, anyhow, bail, ensure};
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::io::{Cursor, ErrorKind, Read};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const GVCP_PORT: u16 = 3956;
const DISCOVERY: u16 = 0x0002;
const PACKET_RESEND: u16 = 0x0040;
const READ_REGISTER: u16 = 0x0080;
const WRITE_REGISTER: u16 = 0x0082;
const READ_MEMORY: u16 = 0x0084;
const WRITE_MEMORY: u16 = 0x0086;
const PENDING_ACK: u16 = 0x0089;
const CONTROL_PRIVILEGE: u32 = 0x0a00;
const GVCP_CAPABILITY: u32 = 0x0934;
const HEARTBEAT_TIMEOUT: u32 = 0x0938;
const TIMESTAMP_HIGH: u32 = 0x093c;
const TIMESTAMP_LOW: u32 = 0x0940;
const STREAM_PORT: u32 = 0x0d00;
const STREAM_PACKET_SIZE: u32 = 0x0d04;
const STREAM_ADDRESS: u32 = 0x0d18;
const SCPS_FIRE: u32 = 0x8000_0000;
const SCPS_NO_FRAGMENT: u32 = 0x4000_0000;
const SCPS_BIG_ENDIAN: u32 = 0x2000_0000;
const PACKET_SIZE_MAX: u32 = 9000;
const TEST_WAIT: Duration = Duration::from_millis(10);
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

fn be16(bytes: &[u8], offset: usize) -> u16 {
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
fn bounded_timeout(timeout: Duration) -> Duration {
    timeout.clamp(Duration::from_millis(1), Duration::from_secs(30))
}
fn is_receive_timeout(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}
fn command_packet(command: u16, id: u16, flags: u8, payload: &[u8]) -> Vec<u8> {
    let mut packet = Vec::with_capacity(8 + payload.len());
    packet.extend([0x42, flags]);
    packet.extend(command.to_be_bytes());
    packet.extend((payload.len() as u16).to_be_bytes());
    packet.extend(id.to_be_bytes());
    packet.extend(payload);
    packet
}
fn resend_packet(id: u16, block: u64, extended: bool, first: u32, last: u32) -> Vec<u8> {
    let range = [first.to_be_bytes(), last.to_be_bytes()].concat();
    let payload = if extended {
        [&[0; 4][..], &range, &block.to_be_bytes()].concat()
    } else {
        [&[0, 0][..], &(block as u16).to_be_bytes(), &range].concat()
    };
    command_packet(PACKET_RESEND, id, if extended { 0x10 } else { 0 }, &payload)
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

#[derive(Debug)]
struct GvcpStatus(u16);
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

fn fixed_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes.split(|b| *b == 0).next().unwrap_or_default())
        .trim()
        .to_owned()
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
    let mac = ((be32(payload, 8) as u64 & 0xffff) << 32) | be32(payload, 12) as u64;
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
    })
}

/// Discover cameras on every usable IPv4 interface, within one shared deadline.
/// A directed broadcast is sent from a socket bound to each interface so that
/// multi-NIC hosts do not silently discover only the default-route network.
pub fn discover(timeout: Duration) -> Result<Vec<CameraInfo>> {
    let deadline = Instant::now() + bounded_timeout(timeout);
    let request = command_packet(DISCOVERY, 0xffff, 0x11, &[]);
    let mut sockets = Vec::new();
    let mut local_addresses = HashSet::new();
    for interface in if_addrs::get_if_addrs().context("enumerating IPv4 interfaces")? {
        let if_addrs::IfAddr::V4(v4) = interface.addr else {
            continue;
        };
        if v4.ip.is_unspecified() || !local_addresses.insert(v4.ip) {
            continue;
        }
        let Ok(socket) = UdpSocket::bind(SocketAddrV4::new(v4.ip, 0)) else {
            continue;
        };
        if socket.set_broadcast(true).is_err() || socket.set_nonblocking(true).is_err() {
            continue;
        }
        // Probe the local interface too: camera simulators and software GigE
        // producers often bind to a unicast or loopback interface.
        let mut sent = socket
            .send_to(&request, SocketAddrV4::new(v4.ip, GVCP_PORT))
            .is_ok();
        if let Some(broadcast) = v4.broadcast {
            sent |= socket
                .send_to(&request, SocketAddrV4::new(broadcast, GVCP_PORT))
                .is_ok();
        }
        if sent {
            sockets.push(socket);
        }
    }
    // This also covers interface APIs that omit directed broadcast addresses.
    if let Ok(socket) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
        && socket.set_broadcast(true).is_ok()
        && socket.set_nonblocking(true).is_ok()
        && socket
            .send_to(&request, (Ipv4Addr::BROADCAST, GVCP_PORT))
            .is_ok()
    {
        sockets.push(socket);
    }
    ensure!(
        !sockets.is_empty(),
        "no usable IPv4 broadcast socket; check network interfaces"
    );
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
                            cameras.insert(info.id.clone(), info);
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
    Ok(cameras.into_values().collect())
}

struct Control {
    socket: UdpSocket,
    timeout: Duration,
    next_id: u16,
}
impl Control {
    fn connect(address: SocketAddrV4, timeout: Duration) -> Result<Self> {
        let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).context("opening GVCP socket")?;
        socket
            .connect(address)
            .context("routing GVCP socket to camera")?;
        Ok(Self {
            socket,
            timeout: bounded_timeout(timeout),
            next_id: 1,
        })
    }
    fn transact(&mut self, command: u16, payload: &[u8]) -> Result<Vec<u8>> {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let packet = command_packet(command, id, 1, payload);
        let deadline = Instant::now() + self.timeout;
        let retry_wait = (self.timeout / 3).max(Duration::from_millis(1));
        let mut buffer = [0u8; 2048];
        for attempt in 0..3 {
            if Instant::now() >= deadline {
                break;
            }
            self.socket.send(&packet).context("sending GVCP command")?;
            let mut attempt_deadline = (Instant::now() + retry_wait).min(deadline);
            loop {
                let remaining = attempt_deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                self.socket.set_read_timeout(Some(remaining))?;
                match self.socket.recv(&mut buffer) {
                    Ok(length) => {
                        // Unrelated, stale and truncated packets cannot complete
                        // a command, nor extend its absolute deadline.
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
                            ensure!(ack.payload.len() == 4, "invalid pending acknowledgement");
                            let extension = Duration::from_millis(be32(ack.payload, 0) as u64);
                            attempt_deadline = (Instant::now() + extension).min(deadline);
                            continue;
                        }
                        return Ok(ack.payload.to_vec());
                    }
                    Err(error) if is_receive_timeout(&error) => break,
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error).context("receiving GVCP acknowledgement"),
                }
            }
            if attempt == 2 {
                break;
            }
        }
        bail!(
            "GVCP command 0x{command:04x} timed out after {:?} (three attempts); check camera address, network and firewall",
            self.timeout
        )
    }
    fn read_register(&mut self, address: u32) -> Result<u32> {
        ensure!(address.is_multiple_of(4), "unaligned GVCP register address");
        let ack = self.transact(READ_REGISTER, &address.to_be_bytes())?;
        ensure!(ack.len() == 4, "invalid register read acknowledgement");
        Ok(be32(&ack, 0))
    }
    fn write_register(&mut self, address: u32, value: u32) -> Result<()> {
        ensure!(address.is_multiple_of(4), "unaligned GVCP register address");
        let mut payload = address.to_be_bytes().to_vec();
        payload.extend(value.to_be_bytes());
        let ack = self.transact(WRITE_REGISTER, &payload)?;
        ensure!(ack.len() == 4, "invalid register write acknowledgement");
        // The data index identifies the number of register pairs processed.
        ensure!(
            be32(&ack, 0) == 1,
            "camera did not acknowledge the register write"
        );
        Ok(())
    }
    fn release(&mut self, heartbeat_timeout: u32) {
        let _ = self.write_register(HEARTBEAT_TIMEOUT, heartbeat_timeout);
        let _ = self.write_register(CONTROL_PRIVILEGE, 0);
    }
    fn read_aligned(&mut self, address: u32, length: usize) -> Result<Vec<u8>> {
        ensure!(
            address.is_multiple_of(4) && length.is_multiple_of(4),
            "unaligned GVCP memory read"
        );
        if length == 4 {
            return Ok(self.read_register(address)?.to_be_bytes().to_vec());
        }
        let mut result = Vec::with_capacity(length);
        for offset in (0..length).step_by(MEMORY_CHUNK) {
            let size = (length - offset).min(MEMORY_CHUNK);
            let current = address
                .checked_add(offset as u32)
                .context("GVCP address overflow")?;
            let mut request = current.to_be_bytes().to_vec();
            request.extend((size as u32).to_be_bytes());
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
        for (index, bytes) in data.chunks(MEMORY_CHUNK).enumerate() {
            let current = address
                .checked_add((index * MEMORY_CHUNK) as u32)
                .context("GVCP address overflow")?;
            if bytes.len() == 4 {
                self.write_register(current, be32(bytes, 0))?;
            } else {
                let mut payload = current.to_be_bytes().to_vec();
                payload.extend(bytes);
                match self.transact(WRITE_MEMORY, &payload) {
                    Ok(ack) => {
                        ensure!(
                            ack.len() == 4 && be32(&ack, 0) == current,
                            "invalid memory write acknowledgement"
                        );
                    }
                    Err(error)
                        if error
                            .downcast_ref::<GvcpStatus>()
                            .is_some_and(|s| s.0 == 0x8001) =>
                    {
                        // WRITE_MEMORY is optional in older GigE Vision devices.
                        for (word, value) in bytes.as_chunks::<4>().0.iter().enumerate() {
                            self.write_register(current + word as u32 * 4, be32(value, 0))?;
                        }
                    }
                    Err(error) => return Err(error),
                }
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
    ensure!(
        address <= u32::MAX as u64,
        "GigE Vision 1.x memory addresses are limited to 32 bits"
    );
    if length == 0 {
        return Ok((address as u32, 0, 0));
    }
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
        base as u32,
        (aligned_end - base) as usize,
        (address - base) as usize,
    ))
}

/// Probe a known IPv4 camera without waiting for LAN-wide discovery.
pub fn probe(ip: Ipv4Addr, timeout: Duration) -> Result<CameraInfo> {
    ensure!(
        !ip.is_unspecified() && !ip.is_multicast() && ip != Ipv4Addr::BROADCAST,
        "expected a unicast camera IPv4 address"
    );
    let timeout = bounded_timeout(timeout);
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
        info.model = "GigE Vision camera".to_owned();
    }
    Ok(info)
}

struct Heartbeat {
    shutdown: Arc<(Mutex<bool>, Condvar)>,
    failure: Arc<Mutex<Option<String>>>,
    thread: Option<JoinHandle<()>>,
}
impl Heartbeat {
    fn start(control: Arc<Mutex<Control>>, period: Duration) -> Result<Self> {
        let shutdown = Arc::new((Mutex::new(false), Condvar::new()));
        let failure = Arc::new(Mutex::new(None));
        let worker_shutdown = shutdown.clone();
        let worker_failure = failure.clone();
        let thread = thread::Builder::new()
            .name("capturefab-gige-heartbeat".into())
            .spawn(move || {
                let (lock, wake) = &*worker_shutdown;
                loop {
                    let stop = lock.lock().unwrap_or_else(|e| e.into_inner());
                    let (stop, _) = wake
                        .wait_timeout_while(stop, period, |stop| !*stop)
                        .unwrap_or_else(|e| e.into_inner());
                    if *stop {
                        break;
                    }
                    drop(stop);
                    let result = control
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .read_register(CONTROL_PRIVILEGE);
                    let error = match result {
                        Ok(value) if value & 3 != 0 => None,
                        Ok(_) => Some("camera controller privilege was lost".to_owned()),
                        Err(error) => Some(format!("camera heartbeat failed: {error:#}")),
                    };
                    *worker_failure.lock().unwrap_or_else(|e| e.into_inner()) = error;
                }
            })
            .context("starting GigE heartbeat thread")?;
        Ok(Self {
            shutdown,
            failure,
            thread: Some(thread),
        })
    }
    fn check(&self) -> Result<()> {
        if let Some(error) = &*self
            .failure
            .lock()
            .map_err(|_| anyhow!("heartbeat mutex poisoned"))?
        {
            bail!("{error}");
        }
        Ok(())
    }
    fn shutdown(&mut self) {
        let (lock, wake) = &*self.shutdown;
        *lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
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

struct GigE {
    control: Arc<Mutex<Control>>,
    heartbeat: Heartbeat,
    camera_ip: Ipv4Addr,
    host_ip: Ipv4Addr,
    timestamp_frequency: u64,
    heartbeat_timeout: u32,
    resend: bool,
    packet_size: Option<Option<u32>>,
    stats: TransportStats,
    stream: Option<Stream>,
    xml_cache: Option<String>,
}

/// Acquire camera control. An independent heartbeat keeps the connection alive
/// even while the GUI is idle or waiting for an external trigger.
pub fn open(info: &CameraInfo, timeout: Duration) -> Result<Box<dyn Backend>> {
    let ip: Ipv4Addr = info
        .address
        .as_deref()
        .context("GigE camera has no address")?
        .parse()
        .context("invalid camera IPv4 address")?;
    Ok(Box::new(connect(
        SocketAddrV4::new(ip, GVCP_PORT),
        timeout,
    )?))
}
fn connect(address: SocketAddrV4, timeout: Duration) -> Result<GigE> {
    let mut control = Control::connect(address, timeout)?;
    let host_ip = match control.socket.local_addr()? {
        SocketAddr::V4(address) => *address.ip(),
        _ => unreachable!(),
    };
    ensure!(
        control.read_register(CONTROL_PRIVILEGE)? & 3 == 0,
        "camera is controlled by another application; disconnect it first"
    );
    control
        .write_register(CONTROL_PRIVILEGE, 2)
        .context("acquiring camera controller privilege")?;
    // Initialization after acquisition must release privilege on every error.
    let initialized = (|| -> Result<(u32, Duration, u64, bool)> {
        ensure!(
            control.read_register(CONTROL_PRIVILEGE)? & 3 != 0,
            "camera did not grant controller privilege"
        );
        let heartbeat_timeout = control.read_register(HEARTBEAT_TIMEOUT)?;
        let heartbeat_ms = heartbeat_timeout
            .max(3000)
            .max(control.timeout.as_millis() as u32 * 2 + 1000);
        control.write_register(HEARTBEAT_TIMEOUT, heartbeat_ms)?;
        let timestamp_frequency = match (
            control.read_register(TIMESTAMP_HIGH),
            control.read_register(TIMESTAMP_LOW),
        ) {
            (Ok(high), Ok(low)) => ((high as u64) << 32) | low as u64,
            _ => 0,
        };
        let resend = control
            .read_register(GVCP_CAPABILITY)
            .is_ok_and(|bits| bits & 0x4 != 0);
        Ok((
            heartbeat_timeout,
            Duration::from_millis((heartbeat_ms / 3).clamp(100, 1000) as u64),
            timestamp_frequency,
            resend,
        ))
    })();
    let (heartbeat_timeout, period, timestamp_frequency, resend) = match initialized {
        Ok(values) => values,
        Err(error) => {
            let _ = control.write_register(CONTROL_PRIVILEGE, 0);
            return Err(error);
        }
    };
    let control = Arc::new(Mutex::new(control));
    let heartbeat = match Heartbeat::start(control.clone(), period) {
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
        packet_size: None,
        stats: TransportStats::default(),
        stream: None,
        xml_cache: None,
    })
}

impl RegisterIo for GigE {
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
    fn xml(&mut self) -> Result<String> {
        if let Some(xml) = &self.xml_cache {
            return Ok(xml.clone());
        }
        let mut errors = Vec::new();
        for url_address in [0x200, 0x400] {
            let result = (|| -> Result<String> {
                let url = fixed_text(&self.read_memory(url_address, 512)?);
                let local = parse_local_url(&url)?;
                let data = self.read_memory(local.address, local.length)?;
                decode_xml(&local.name, data)
            })();
            match result {
                Ok(xml) => {
                    self.xml_cache = Some(xml.clone());
                    return Ok(xml);
                }
                Err(error) => errors.push(format!("URL at 0x{url_address:x}: {error:#}")),
            }
        }
        bail!("unable to load camera GenICam XML: {}", errors.join("; "))
    }
    fn start(&mut self, payload_size: usize) -> Result<()> {
        self.heartbeat.check()?;
        ensure!(self.stream.is_none(), "GigE receiving is already started");
        ensure!(
            payload_size > 0 && payload_size <= MAX_PAYLOAD,
            "camera payload size must be 1..=128 MiB"
        );
        let socket = UdpSocket::bind((self.host_ip, 0)).context("opening GVSP receive socket")?;
        let receive = socket2::SockRef::from(&socket);
        request_receive_buffer(payload_size, |size| receive.set_recv_buffer_size(size));
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
        let result = (|| -> Result<(u32, bool)> {
            control.write_register(STREAM_ADDRESS, u32::from(self.host_ip))?;
            control.write_register(STREAM_PORT, port as u32)?;
            let original = control.read_register(STREAM_PACKET_SIZE)?;
            let mut test = |size| {
                probe_packet_size(&mut control, &socket, &mut buffer, camera, original, size)
            };
            let negotiated = match self.packet_size {
                Some(None) => None,
                cached => cached
                    .flatten()
                    .and_then(&mut test)
                    .or_else(|| ladder(&mut test, cap, mtu.is_some())),
            };
            self.packet_size = Some(negotiated);
            control.write_register(
                STREAM_PACKET_SIZE,
                (original & (SCPS_NO_FRAGMENT | SCPS_BIG_ENDIAN))
                    | negotiated.unwrap_or(cap.min(1500)),
            )?;
            let packet_size = control.read_register(STREAM_PACKET_SIZE)? & 0xffff;
            ensure!(
                (576..=PACKET_SIZE_MAX).contains(&packet_size),
                "camera selected unsupported stream packet size {packet_size}"
            );
            drain(&socket, &mut buffer);
            socket.set_read_timeout(Some(POLL))?;
            Ok((packet_size, negotiated.is_some()))
        })();
        match result {
            Ok((packet_size, negotiated)) => {
                let mut notes: Vec<String> = receive_buffer
                    .and_then(|bytes| receive_buffer_note(bytes, payload_size))
                    .into_iter()
                    .collect();
                if !negotiated {
                    notes.push(format!(
                        "camera sent no test packet; if frames time out, allow inbound UDP from {}",
                        self.camera_ip
                    ));
                }
                let mut assembler =
                    Assembler::new(payload_size, packet_size as usize, self.timestamp_frequency);
                assembler.resend = self.resend;
                assembler.stats = TransportStats {
                    packet_size: Some(packet_size),
                    receive_buffer_bytes: receive_buffer.map(|bytes| bytes as u64),
                    notes,
                    ..std::mem::take(&mut self.stats)
                };
                self.stream = Some(Stream {
                    socket,
                    gvcp,
                    buffer,
                    timeout: POLL,
                    next_poll: Instant::now(),
                    request_id: 0,
                    assembler,
                });
                Ok(())
            }
            Err(error) => {
                let _ = control.write_register(STREAM_PORT, 0);
                Err(error)
            }
        }
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
                return Ok(frame);
            }
            if now >= stream.next_poll {
                self.heartbeat.check()?;
                stream.assembler.poll(now);
                stream.flush();
                stream.next_poll = now + POLL;
                continue;
            }
            let remaining = deadline.saturating_duration_since(now);
            if remaining.is_zero() {
                break;
            }
            let wait = remaining.min(POLL);
            if (wait < POLL) != (stream.timeout < POLL) {
                stream.socket.set_read_timeout(Some(wait))?;
                stream.timeout = wait;
            }
            let received = stream.socket.recv_from(&mut stream.buffer);
            now = Instant::now();
            match received {
                Ok((length, source)) if source.ip() == camera && source != stream.gvcp => {
                    let result = stream.assembler.push_at(&stream.buffer[..length], now);
                    stream.flush();
                    match result {
                        Ok(Some(frame)) => return Ok(frame),
                        Ok(None) => {}
                        Err(error) => last_error = Some(error.to_string()),
                    }
                }
                Ok(_) => {}
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
    fn stats(&self) -> Option<TransportStats> {
        Some(
            self.stream
                .as_ref()
                .map_or(&self.stats, |stream| &stream.assembler.stats)
                .clone(),
        )
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
    if !cfg!(target_os = "linux") {
        return None;
    }
    let interface = if_addrs::get_if_addrs()
        .ok()?
        .into_iter()
        .find(|interface| interface.ip() == IpAddr::V4(host))?;
    std::fs::read_to_string(format!("/sys/class/net/{}/mtu", interface.name))
        .ok()?
        .trim()
        .parse()
        .ok()
}
fn drain(socket: &UdpSocket, buffer: &mut [u8]) {
    if socket.set_nonblocking(true).is_ok() {
        while socket.recv(buffer).is_ok() {}
        let _ = socket.set_nonblocking(false);
    }
}
fn probe_packet_size(
    control: &mut Control,
    socket: &UdpSocket,
    buffer: &mut [u8],
    camera: IpAddr,
    original: u32,
    size: u32,
) -> Option<u32> {
    drain(socket, buffer);
    let mut readback = None;
    for _ in 0..3 {
        let value = (original & SCPS_BIG_ENDIAN) | SCPS_FIRE | SCPS_NO_FRAGMENT | size;
        control.write_register(STREAM_PACKET_SIZE, value).ok()?;
        let tested = match readback {
            Some(tested) => tested,
            None => *readback.insert(control.read_register(STREAM_PACKET_SIZE).ok()? & 0xffff),
        };
        let deadline = Instant::now() + TEST_WAIT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() || socket.set_read_timeout(Some(remaining)).is_err() {
                break;
            }
            match socket.recv_from(buffer) {
                Ok((length, source)) if source.ip() == camera && length + 28 == tested as usize => {
                    return Some(tested);
                }
                Err(error) if is_receive_timeout(&error) => break,
                _ => {}
            }
        }
    }
    None
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
        let middle = ((low + high) / 2) & !3;
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

struct LocalUrl {
    name: String,
    address: u64,
    length: usize,
}
fn parse_local_url(url: &str) -> Result<LocalUrl> {
    let (scheme, remainder) = url
        .split_once(':')
        .context("empty or malformed GenICam URL")?;
    ensure!(
        scheme.eq_ignore_ascii_case("local"),
        "unsupported XML URL scheme {scheme:?}; camera must expose a Local: XML URL"
    );
    let mut parts = remainder.rsplitn(3, ';');
    let length = parts.next().context("missing local XML length")?;
    let address = parts.next().context("missing local XML address")?;
    let name = parts.next().context("missing local XML filename")?;
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
    let bytes = if name.to_ascii_lowercase().ends_with(".zip") || data.starts_with(b"PK\x03\x04") {
        let mut archive =
            zip::ZipArchive::new(Cursor::new(data)).context("opening camera XML ZIP")?;
        ensure!(
            archive.len() <= 128,
            "camera XML ZIP contains too many entries"
        );
        let mut selected = None;
        for index in 0..archive.len() {
            let mut entry = archive.by_index(index)?;
            if entry.is_dir() || !entry.name().to_ascii_lowercase().ends_with(".xml") {
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
    let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&bytes);
    let text = std::str::from_utf8(bytes)
        .context("camera XML is not UTF-8")?
        .trim_end_matches('\0');
    ensure!(
        text.trim_start().starts_with('<'),
        "camera XML does not contain an XML document"
    );
    Ok(text.to_owned())
}

struct Stream {
    socket: UdpSocket,
    gvcp: SocketAddr,
    buffer: Vec<u8>,
    timeout: Duration,
    next_poll: Instant,
    request_id: u16,
    assembler: Assembler,
}
impl Stream {
    fn flush(&mut self) {
        for (block, extended, first, last) in self.assembler.requests.drain(..) {
            self.request_id = self.request_id.wrapping_add(1).max(1);
            let packet = resend_packet(self.request_id, block, extended, first, last);
            let _ = self.socket.send_to(&packet, self.gvcp);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ImageLeader {
    width: u32,
    height: u32,
    pixel_format: u32,
    timestamp: u64,
    row_bytes: usize,
    x_padding: usize,
    y_padding: usize,
    wire_bytes: usize,
}
impl ImageLeader {
    fn parse(data: &[u8], payload_limit: usize) -> Result<Self> {
        ensure!(data.len() >= 36, "truncated GVSP image leader");
        ensure!(
            be16(data, 2) == 1,
            "unsupported GVSP payload type 0x{:04x}; select image acquisition and disable chunks",
            be16(data, 2)
        );
        let pixel_format = be32(data, 12);
        let width = be32(data, 16);
        let height = be32(data, 20);
        let bits_per_pixel = (pixel_format >> 16) & 0xff;
        ensure!(
            width > 0 && height > 0 && bits_per_pixel > 0 && bits_per_pixel <= 64,
            "invalid GVSP image dimensions or pixel format"
        );
        let row_bytes = (width as u64 * bits_per_pixel as u64).div_ceil(8);
        let x_padding = be16(data, 32) as usize;
        let y_padding = be16(data, 34) as usize;
        let wire_bytes = (row_bytes + x_padding as u64)
            .checked_mul(height as u64)
            .and_then(|v| v.checked_add(y_padding as u64))
            .context("GVSP image size overflow")?;
        ensure!(
            wire_bytes <= payload_limit as u64,
            "GVSP image exceeds configured payload size ({wire_bytes} > {payload_limit})"
        );
        Ok(Self {
            width,
            height,
            pixel_format,
            timestamp: be64(data, 4),
            row_bytes: row_bytes as usize,
            x_padding,
            y_padding,
            wire_bytes: wire_bytes as usize,
        })
    }
}

struct GvspPacket<'a> {
    status: u16,
    frame: u64,
    id: u32,
    kind: u8,
    extended: bool,
    data: &'a [u8],
}
fn parse_gvsp(packet: &[u8]) -> Result<GvspPacket<'_>> {
    ensure!(packet.len() >= 8, "truncated GVSP header");
    let info = be32(packet, 4);
    let extended = info & 0x8000_0000 != 0;
    let (frame, id, header) = if extended {
        ensure!(packet.len() >= 20, "truncated extended GVSP header");
        (be64(packet, 8), be32(packet, 16), 20)
    } else {
        (be16(packet, 2) as u64, info & 0x00ff_ffff, 8)
    };
    Ok(GvspPacket {
        status: be16(packet, 0),
        frame,
        id,
        kind: ((info >> 24) & 0x7f) as u8,
        extended,
        data: &packet[header..],
    })
}

struct PartialFrame {
    block: u64,
    extended: bool,
    last_packet: Instant,
    leader: Option<ImageLeader>,
    trailer: Option<(u32, u32)>,
    packets: Option<usize>,
    data: Vec<u8>,
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
        self.leader.is_some() && Some(self.count) == self.packets
    }
}
fn bit(bits: &[u64], id: u32) -> bool {
    bits.get(id as usize / 64)
        .is_some_and(|word| (word >> (id % 64)) & 1 != 0)
}
fn set_bit(bits: &mut [u64], id: u32) {
    bits[id as usize / 64] |= 1 << (id % 64);
}
fn missing(received: &[u64], packets: Option<usize>) -> Vec<(u32, u32)> {
    let end = packets.map_or(received.len() * 64, |packets| packets + 2) as u32;
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
fn validate_fragment(
    leader: &ImageLeader,
    block_size: usize,
    id: u32,
    length: usize,
) -> Result<()> {
    let packet_count = leader.wire_bytes.div_ceil(block_size);
    ensure!(
        id > 0 && id as usize <= packet_count,
        "GVSP payload packet lies beyond image"
    );
    let expected = if id as usize == packet_count {
        leader.wire_bytes - (packet_count - 1) * block_size
    } else {
        block_size
    };
    ensure!(
        length == expected,
        "GVSP payload packet has wrong length ({length} != {expected})"
    );
    Ok(())
}
fn check_trailer(leader: &ImageLeader, packets: usize, (id, height): (u32, u32)) -> Result<()> {
    ensure!(
        id as usize == packets + 1,
        "GVSP trailer packet ID does not match image size"
    );
    ensure!(
        height == leader.height,
        "GVSP trailer height differs from leader"
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
        let granted = self.tokens >= packets as f64;
        if granted {
            self.tokens -= packets as f64;
        }
        granted
    }
}
fn block_distance(from: u64, to: u64, extended: bool) -> u64 {
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
        let window = if finished >= 128 {
            u128::MAX
        } else {
            (1 << finished) - 1
        };
        let lost = finished - u64::from((self.seen & window).count_ones());
        self.seen =
            self.seen.checked_shr(finished as u32).unwrap_or(0) | (1 << (offset - finished));
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
    budget: Budget,
    blocks: Blocks,
    stats: TransportStats,
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
            budget: Budget {
                tokens: RESEND_BUDGET,
                at: Instant::now(),
            },
            blocks: Blocks::default(),
            stats: TransportStats::default(),
        }
    }
    fn incomplete_summary(&self) -> String {
        if let Some(frame) = self.frames.last() {
            format!(
                "{} incomplete frame(s), newest has {} bytes and {} packets, leader {}, trailer {}; {} expired/evicted frame(s)",
                self.frames.len(),
                frame.bytes,
                frame.count,
                frame.leader.is_some(),
                frame.trailer.is_some(),
                self.stats.incomplete_frames
            )
        } else {
            format!(
                "no complete image received; {} expired/evicted frame(s); check AcquisitionStart, trigger mode and stream firewall",
                self.stats.incomplete_frames
            )
        }
    }
    #[cfg(test)]
    fn push(&mut self, packet: &[u8]) -> Result<Option<Frame>> {
        self.push_at(packet, Instant::now())
    }
    fn push_at(&mut self, packet: &[u8], now: Instant) -> Result<Option<Frame>> {
        let packet = parse_gvsp(packet)?;
        self.stats.packets_received += 1;
        let block = packet.frame;
        let index = self.frames.iter().rposition(|frame| frame.block == block);
        if index.is_none() && self.closed.contains(&block) {
            return Ok(self.ready.pop_front());
        }
        if packet.status >= 0x8000 {
            self.discard(block);
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
        ensure!(
            (1..=3).contains(&packet.kind),
            "unsupported GVSP packet content {}; multipart/GenDC/all-in packets are not supported",
            packet.kind
        );
        let (block_size, max_packets) =
            self.limits[packet.extended as usize].context("invalid GVSP packet size")?;
        ensure!(
            packet.id as usize <= max_packets + 1,
            "GVSP packet ID exceeds payload bounds"
        );
        let index = match index {
            Some(index) => index,
            None => self.create(packet.frame, packet.extended, max_packets, now),
        };
        let frame = &mut self.frames[index];
        ensure!(
            frame.extended == packet.extended,
            "GVSP frame mixes standard and extended IDs"
        );
        frame.last_packet = now;
        match packet.kind {
            1 => {
                ensure!(packet.id == 0, "GVSP leader packet ID must be zero");
                let leader = ImageLeader::parse(packet.data, self.payload_limit)?;
                if let Some(previous) = &frame.leader {
                    ensure!(previous == &leader, "conflicting duplicate GVSP leader");
                } else {
                    let packets = leader.wire_bytes.div_ceil(block_size);
                    ensure!(
                        packets <= MAX_FRAME_PACKETS,
                        "GVSP image requires too many packets"
                    );
                    let short = if bit(&frame.received, packets as u32) {
                        packets * block_size - leader.wire_bytes
                    } else {
                        0
                    };
                    ensure!(
                        frame.end <= leader.wire_bytes
                            && frame.bytes + short == frame.count * block_size,
                        "GVSP payload packets do not match the image leader"
                    );
                    if let Some(trailer) = frame.trailer {
                        check_trailer(&leader, packets, trailer)?;
                    }
                    frame.packets = Some(packets);
                    frame.leader = Some(leader);
                    set_bit(&mut frame.received, 0);
                }
            }
            2 => {
                ensure!(
                    packet.id > 1 && packet.data.len() >= 8,
                    "invalid GVSP image trailer"
                );
                ensure!(be32(packet.data, 0) == 1, "GVSP trailer is not an image");
                let trailer = (packet.id, be32(packet.data, 4));
                if let Some(previous) = frame.trailer {
                    ensure!(previous == trailer, "conflicting duplicate GVSP trailer");
                } else {
                    if let (Some(leader), Some(packets)) = (&frame.leader, frame.packets) {
                        check_trailer(leader, packets, trailer)?;
                    }
                    frame.packets.get_or_insert(packet.id as usize - 1);
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
                    offset + length <= self.payload_limit,
                    "GVSP payload exceeds configured bounds"
                );
                if let Some(leader) = &frame.leader {
                    validate_fragment(leader, block_size, packet.id, length)?;
                }
                let range = offset..offset + length;
                if bit(&frame.received, packet.id) {
                    ensure!(
                        frame.data[range] == *packet.data,
                        "conflicting duplicate GVSP payload packet"
                    );
                } else {
                    if frame.data.is_empty() {
                        frame.data = vec![0; self.payload_limit];
                    }
                    frame.data[range].copy_from_slice(packet.data);
                    set_bit(&mut frame.received, packet.id);
                    frame.count += 1;
                    frame.bytes += length;
                    frame.end = frame.end.max(offset + length);
                    if bit(&frame.requested, packet.id) {
                        self.stats.resend_recovered += 1;
                    }
                }
            }
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
            packets: None,
            data: Vec::new(),
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
    fn poll(&mut self, now: Instant) {
        let idle = |frame: &PartialFrame| now.saturating_duration_since(frame.last_packet);
        while let Some(index) = self.frames.iter().position(|frame| {
            idle(frame) >= FRAME_IDLE
                || (!frame.due && idle(frame) >= RESEND_IDLE)
                || (frame.rounds > 0
                    && now.saturating_duration_since(frame.requested_at) >= RESEND_RETRY)
        }) {
            let frame = &self.frames[index];
            if idle(frame) >= FRAME_IDLE || frame.rounds >= RESEND_ROUNDS {
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
        let frame = &self.frames[index];
        let runs = missing(&frame.received, frame.packets);
        let total: u32 = runs.iter().map(|(first, last)| last - first + 1).sum();
        let limit = (frame.packets.unwrap_or(frame.count) / 8).max(16);
        if frame.rounds == 0 && (runs.len() > RESEND_RUNS || total as usize > limit) {
            self.abandon(index);
            return;
        }
        let frame = &mut self.frames[index];
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
        let leader = frame.leader.expect("complete frames have a leader");
        let mut data = frame.data;
        let stride = leader.row_bytes + leader.x_padding;
        if leader.x_padding > 0 {
            for row in 1..leader.height as usize {
                data.copy_within(
                    row * stride..row * stride + leader.row_bytes,
                    row * leader.row_bytes,
                );
            }
        }
        data.truncate(leader.row_bytes * leader.height as usize);
        let timestamp_ns = if self.timestamp_frequency == 0 {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
                .min(u64::MAX as u128) as u64
        } else {
            ((leader.timestamp as u128 * 1_000_000_000) / self.timestamp_frequency as u128)
                .min(u64::MAX as u128) as u64
        };
        self.ready.push_back(Frame {
            id: frame.block,
            width: leader.width,
            height: leader.height,
            pixel_format: leader.pixel_format,
            timestamp_ns,
            data,
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
        result.extend((data.len() as u16).to_be_bytes());
        result.extend(id.to_be_bytes());
        result.extend(data);
        result
    }
    fn gvsp(frame: u64, id: u32, kind: u8, extended: bool, data: &[u8]) -> Vec<u8> {
        let mut result = vec![0, 0];
        if extended {
            result.extend([0, 0]);
            result.extend((0x8000_0000 | ((kind as u32) << 24)).to_be_bytes());
            result.extend(frame.to_be_bytes());
            result.extend(id.to_be_bytes());
        } else {
            result.extend((frame as u16).to_be_bytes());
            result.extend((((kind as u32) << 24) | id).to_be_bytes());
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
        (0..width * height).map(|i| (i * 7) as u8).collect()
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
            packets.push(gvsp(block, packets.len() as u32, 3, extended, chunk));
        }
        packets.push(gvsp(
            block,
            packets.len() as u32,
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

    const WIDTH: u32 = 200;
    const HEIGHT: u32 = 40;
    #[derive(Default)]
    struct FakeCamera {
        registers: HashMap<u32, u32>,
        unreadable: Vec<u32>,
        path_mtu: u32,
        test_packets: bool,
        stores_flags: bool,
        stale: bool,
        clamp: Option<u32>,
        reject_above: Option<u32>,
        resend_status: u16,
        frames: HashMap<u64, Vec<Vec<u8>>>,
        requests: Vec<(Vec<u8>, u16)>,
        scps_writes: Vec<u32>,
    }
    impl FakeCamera {
        fn register(&self, address: u32) -> u32 {
            self.registers.get(&address).copied().unwrap_or_default()
        }
        fn destination(&self) -> SocketAddrV4 {
            SocketAddrV4::new(
                Ipv4Addr::from(self.register(STREAM_ADDRESS)),
                self.register(STREAM_PORT) as u16,
            )
        }
        fn write(&mut self, address: u32, value: u32, stream: &UdpSocket) -> u16 {
            if address != STREAM_PACKET_SIZE {
                self.registers.insert(address, value);
                return 0;
            }
            self.scps_writes.push(value);
            if self
                .reject_above
                .is_some_and(|limit| value & 0xffff > limit)
            {
                return 0x8002;
            }
            let size = self
                .clamp
                .map_or(value & 0xffff, |clamp| clamp.min(value & 0xffff));
            if value & SCPS_FIRE != 0 && self.test_packets {
                let stale = vec![0; size as usize - 29];
                let fired = vec![0; size as usize - 28];
                for (packet, send) in [
                    (&stale, self.stale),
                    (&fired, size <= self.path_mtu),
                    (&stale, self.stale),
                ] {
                    if send {
                        let _ = stream.send_to(packet, self.destination());
                    }
                }
            }
            let flags = value
                & if self.stores_flags {
                    0xffff_0000
                } else {
                    0x7fff_0000
                };
            self.registers.insert(address, flags | size);
            0
        }
        fn resend(&mut self, packet: &[u8], port: u16, stream: &UdpSocket) {
            self.requests.push((packet.to_vec(), port));
            let extended = packet[1] & 0x10 != 0;
            let block = if extended {
                be64(packet, 20)
            } else {
                be16(packet, 10) as u64
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
                let _ = stream.send_to(&resent, self.destination());
            }
        }
    }
    struct Fake {
        address: SocketAddrV4,
        gvcp: UdpSocket,
        stream: UdpSocket,
        camera: Arc<Mutex<FakeCamera>>,
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    }
    impl Fake {
        fn new(setup: impl FnOnce(&mut FakeCamera)) -> Self {
            let mut camera = FakeCamera::default();
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
                    let mut camera = state.lock().unwrap();
                    let reply = match command {
                        READ_REGISTER => {
                            let addresses: Vec<u32> =
                                packet[8..].chunks(4).map(|a| be32(a, 0)).collect();
                            if addresses.iter().any(|a| camera.unreadable.contains(a)) {
                                ack(command + 1, id, 0x8003, &[])
                            } else {
                                let values: Vec<u8> = addresses
                                    .iter()
                                    .flat_map(|a| camera.register(*a).to_be_bytes())
                                    .collect();
                                ack(command + 1, id, 0, &values)
                            }
                        }
                        WRITE_REGISTER => {
                            match camera.write(be32(packet, 8), be32(packet, 12), &source) {
                                0 => ack(command + 1, id, 0, &1u32.to_be_bytes()),
                                status => ack(command + 1, id, status, &[]),
                            }
                        }
                        PACKET_RESEND => {
                            camera.resend(packet, peer.port(), &source);
                            continue;
                        }
                        _ => ack(command + 1, id, 0x8001, &[]),
                    };
                    drop(camera);
                    let _ = server.send_to(&reply, peer);
                }
            });
            Self {
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
            let mut camera = self.camera.lock().unwrap();
            let size = (camera.register(STREAM_PACKET_SIZE) & 0xffff) as usize;
            let overhead = if extended { 48 } else { 36 };
            let packets = frame_packets(block, extended, WIDTH, HEIGHT, size - overhead);
            for (id, packet) in packets.iter().enumerate() {
                if !drops.contains(&(id as u32)) {
                    self.stream.send_to(packet, camera.destination()).unwrap();
                }
            }
            camera.frames.insert(block, packets);
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
        let mut gige = connect(fake.address, Duration::from_millis(200)).unwrap();
        gige.packet_size = Some(Some(1500));
        gige.start((WIDTH * HEIGHT) as usize).unwrap();
        (fake, gige)
    }
    fn negotiate(setup: impl FnOnce(&mut FakeCamera), cap: u32, mtu_known: bool) -> Option<u32> {
        let fake = Fake::new(|camera| {
            camera.test_packets = true;
            setup(camera);
        });
        let mut control = Control::connect(fake.address, Duration::from_millis(200)).unwrap();
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = socket.local_addr().unwrap().port();
        control
            .write_register(STREAM_ADDRESS, u32::from(Ipv4Addr::LOCALHOST))
            .unwrap();
        control.write_register(STREAM_PORT, port as u32).unwrap();
        let original = control.read_register(STREAM_PACKET_SIZE).unwrap();
        let mut buffer = vec![0; 65_536];
        let camera = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let size = ladder(
            |size| probe_packet_size(&mut control, &socket, &mut buffer, camera, original, size),
            cap,
            mtu_known,
        );
        let writes = fake.camera.lock().unwrap().scps_writes.clone();
        let fired = SCPS_FIRE | SCPS_NO_FRAGMENT | (original & SCPS_BIG_ENDIAN);
        assert!(writes.iter().all(|value| value & 0xffff_0000 == fired));
        size
    }
    #[test]
    fn discovery_identity_and_truncated_packets() {
        let mut identity = vec![0u8; 0xf8];
        identity[12..16].copy_from_slice(&0x12345678u32.to_be_bytes());
        identity[0x48..0x4c].copy_from_slice(b"Acme");
        identity[0x68..0x6c].copy_from_slice(b"C123");
        identity[0xd8..0xdc].copy_from_slice(b"5678");
        let camera = discovery_info(&identity, Ipv4Addr::new(10, 1, 2, 3)).unwrap();
        assert_eq!(camera.id, "gige:000012345678");
        assert_eq!(camera.address.as_deref(), Some("10.1.2.3"));
        assert_eq!(camera.vendor, "Acme");
        identity[12..16].fill(0);
        let first = discovery_info(&identity, Ipv4Addr::new(127, 0, 0, 1)).unwrap();
        let second = discovery_info(&identity, Ipv4Addr::new(192, 168, 2, 1)).unwrap();
        assert_ne!(
            first.id, second.id,
            "software cameras with no MAC must stay distinct"
        );
        assert!(discovery_info(&identity[..240], Ipv4Addr::LOCALHOST).is_err());
        assert!(parse_ack(&[0; 7]).is_err());
        assert!(parse_ack(&[0, 0, 0, 3, 0, 1, 0, 1]).is_err());
    }
    #[test]
    fn udp_retries_ignore_stale_id_and_wrong_ack_command() {
        let server = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        server
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let address = match server.local_addr().unwrap() {
            SocketAddr::V4(a) => a,
            _ => unreachable!(),
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
    #[test]
    fn udp_status_and_address_validation() {
        for invalid_address in [false, true] {
            let server = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            let address = match server.local_addr().unwrap() {
                SocketAddr::V4(a) => a,
                _ => unreachable!(),
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
    fn local_url_hex_and_memory_ranges() {
        let url = parse_local_url("Local:Camera.xml;0x1000;400").unwrap();
        assert_eq!((url.address, url.length), (4096, 1024));
        assert!(parse_local_url("http://camera.example/xml").is_err());
        assert!(parse_local_url("Local:a.xml;0;2000000").is_err());
        assert_eq!(memory_range(0x101, 5).unwrap(), (0x100, 8, 1));
        assert!(memory_range(u32::MAX as u64, 2).is_err());
        assert_eq!(
            memory_range(u32::MAX as u64, 1).unwrap(),
            (0xffff_fffc, 4, 3)
        );
    }
    #[test]
    fn zip_xml_load_and_ambiguous_entries() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;
        for count in [1, 2] {
            let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
            for index in 0..count {
                writer
                    .start_file(format!("Camera{index}.xml"), SimpleFileOptions::default())
                    .unwrap();
                writer.write_all(b"<RegisterDescription/>").unwrap();
            }
            let data = writer.finish().unwrap().into_inner();
            let result = decode_xml("Camera.zip", data);
            assert_eq!(result.is_ok(), count == 1);
        }
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
            assert_eq!(frame.timestamp_ns, 1_234_000_000);
            assert_eq!(frame.id, id);
            assert!(assembler.push(&first).unwrap().is_none());
        }
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
        assert!(assembler.push(&gvsp(1, 1, 3, false, &[2; 7])).is_err());
        assert!(assembler.frames.is_empty());
        assert!(
            assembler
                .push(&gvsp(2, u32::MAX, 3, true, &[0; 8]))
                .is_err()
        );
        for length in 0..8 {
            assert!(parse_gvsp(&[0; 8][..length]).is_err());
        }
        let mut huge = leader(u32::MAX, u32::MAX, RGB8);
        assert!(ImageLeader::parse(&huge, MAX_PAYLOAD).is_err());
        huge[2..4].copy_from_slice(&0x4001u16.to_be_bytes());
        assert!(ImageLeader::parse(&huge, MAX_PAYLOAD).is_err());
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
    fn packet_resend_commands_match_aravis_captures() {
        let hex = |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
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
            ids.iter().for_each(|id| set_bit(&mut bits, *id));
            bits
        };
        assert_eq!(
            missing(&bits(&[1, 2, 5, 6]), Some(6)),
            [(0, 0), (3, 4), (7, 7)]
        );
        assert!(missing(&bits(&[0, 1, 2, 3, 4, 5, 6, 7]), Some(6)).is_empty());
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
        assert!(assembler.push(&unknown).is_err());
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
        assert!(assembler.requests.is_empty());
        assembler.poll(at(10));
        assert_eq!(assembler.requests, [(1, false, 3, 3), (1, false, 7, 7)]);
        for retry in [30, 50] {
            assembler.requests.clear();
            assembler.poll(at(retry - 1));
            assert!(assembler.requests.is_empty());
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
        assert!(assembler.requests.is_empty());
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
        assert!(assembler.requests.is_empty());
        assembler.poll(at(320));
        assert_eq!(assembler.requests, [(5, false, 3, 3)]);
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
            for (id, packet) in frame_packets(1, false, 8, packets as u32, 8)
                .iter()
                .enumerate()
            {
                if !drops.contains(&(id as u32)) {
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
            .map(|i| u32::MAX as u64 + i)
            .collect();
        assert_eq!(lost(extended, true), 3);
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
    fn direct_placement_handles_reordering_duplicates_padding_and_late_leaders() {
        let mut assembler = Assembler::new(32, 44, 0);
        let mut metadata = leader(5, 3, MONO8);
        metadata[32..34].copy_from_slice(&1u16.to_be_bytes());
        metadata[34..36].copy_from_slice(&2u16.to_be_bytes());
        let wire: Vec<u8> = (1..=20).collect();
        let payload = |id: usize| {
            gvsp(
                1,
                id as u32,
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
        assert!(assembler.push(&gvsp(3, 2, 3, false, &[1; 8])).is_err());
        assembler.push(&gvsp(1, 1, 3, false, &[1; 8])).unwrap();
        assert!(assembler.push(&gvsp(1, 1, 3, false, &[2; 8])).is_err());
        assembler.push(&gvsp(2, 1, 3, false, &[1; 4])).unwrap();
        assembler.push(&gvsp(2, 2, 3, false, &[1; 8])).unwrap();
        assert!(
            assembler
                .push(&gvsp(2, 0, 1, false, &leader(4, 3, MONO8)))
                .is_err()
        );
        assert!(assembler.frames.is_empty());
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
    fn start_keeps_flags_drains_test_packets_and_caches_the_size() {
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
        assert_eq!(
            fake.camera.lock().unwrap().scps_writes,
            [SCPS_FIRE | flags | 9000, flags | 9000]
        );
        let socket = &gige.stream.as_ref().unwrap().socket;
        socket.set_nonblocking(true).unwrap();
        assert!(socket.recv(&mut vec![0; 65_536]).is_err());
        socket.set_nonblocking(false).unwrap();
        assert_eq!(gige.stats().unwrap().packet_size, Some(9000));
        gige.stop().unwrap();
        gige.start(8000).unwrap();
        assert_eq!(fake.camera.lock().unwrap().scps_writes.len(), 4);
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
    fn fake_camera_resends_lost_packets() {
        for (status, extended) in [(0x0100, false), (0x0000, false), (0x0100, true)] {
            let (fake, mut gige) = streaming(|camera| {
                camera.registers.insert(GVCP_CAPABILITY, 0x4);
                camera.resend_status = status;
            });
            let block = if extended { 0x1_0000_0001 } else { 1 };
            fake.send(block, extended, &[3, 7]);
            let frame = gige.next_frame(Duration::from_secs(2)).unwrap();
            assert_eq!((frame.id, frame.data), (block, image(WIDTH, HEIGHT)));
            assert!(gige.next_frame(Duration::from_millis(50)).is_err());
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
    fn frames_survive_a_worker_stall_while_packets_wait_in_the_socket() {
        let (fake, mut gige) = streaming(|camera| {
            camera.registers.insert(GVCP_CAPABILITY, 0x4);
        });
        fake.send(1, false, &[4, 5, 6, 7]);
        assert!(gige.next_frame(Duration::from_millis(5)).is_err());
        let before = gige.stats().unwrap().resend_requested;
        thread::sleep(FRAME_IDLE + Duration::from_millis(50));
        let camera = fake.camera.lock().unwrap();
        for packet in &camera.frames[&1][4..] {
            fake.stream.send_to(packet, camera.destination()).unwrap();
        }
        drop(camera);
        assert_eq!(gige.next_frame(Duration::from_secs(1)).unwrap().id, 1);
        let stats = gige.stats().unwrap();
        assert_eq!(
            (stats.resend_requested, stats.incomplete_frames),
            (before, 0)
        );
    }
    #[test]
    fn fake_camera_unavailable_packets_abandon_the_frame() {
        let (fake, mut gige) = streaming(|camera| {
            camera.registers.insert(GVCP_CAPABILITY, 0x4);
            camera.resend_status = 0x800c;
        });
        fake.send(1, false, &[3, 7]);
        assert!(gige.next_frame(Duration::from_millis(300)).is_err());
        assert!(gige.next_frame(Duration::from_millis(100)).is_err());
        assert_eq!(fake.camera.lock().unwrap().requests.len(), 2);
        let stats = gige.stats().unwrap();
        assert_eq!(
            (
                stats.resend_requested,
                stats.resend_recovered,
                stats.incomplete_frames
            ),
            (2, 0, 1)
        );
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
        assert!(fake.camera.lock().unwrap().requests.is_empty());
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
    fn connect_reads_resend_capability_and_restores_the_heartbeat_timeout() {
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
            assert_eq!(fake.register(HEARTBEAT_TIMEOUT), 3000);
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
}
