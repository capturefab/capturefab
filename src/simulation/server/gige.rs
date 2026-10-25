//! GVCP/GVSP test camera. Layouts match the bootstrap registers consumed by
//! our native transport and the public Aravis protocol headers.
//! https://github.com/AravisProject/aravis/blob/main/src/arvgvcpprivate.h
use super::{LatestFrame, ServerOptions};
use crate::{camera::Camera, types::Frame};
use anyhow::{Context, Result, ensure};
use std::{
    collections::{BTreeMap, VecDeque},
    net::{Ipv4Addr, SocketAddr, UdpSocket},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const SOURCE_BASE: u64 = 0x10000;
const XML_BASE: u64 = 0x100000;
const PRIVILEGE: u32 = 0x0a00;
const HEARTBEAT: u32 = 0x0938;
const STREAM_PORT: u32 = 0x0d00;
const STREAM_SIZE: u32 = 0x0d04;
const STREAM_DELAY: u32 = 0x0d08;
const STREAM_ADDRESS: u32 = 0x0d18;
const USER_NAME: u32 = 0x00e8;
/// Inter-packet delay limit in timestamp ticks (1 GHz): 10 ms.
const MAX_STREAM_DELAY: u32 = 10_000_000;
const MAX_CACHE_BYTES: usize = 32 * 1024 * 1024;
type AcquisitionCommands = BTreeMap<u64, (u64, u64)>;

/// A GVCP acknowledgement status and its reason.
#[derive(Debug)]
struct Status(u16, &'static str);
impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.1)
    }
}
impl std::error::Error for Status {}

struct State {
    registers: BTreeMap<u32, u32>,
    controller: Option<SocketAddr>,
    heartbeat: Instant,
    cache: VecDeque<(u16, Arc<Vec<Vec<u8>>>)>,
}
impl State {
    fn reg(&self, address: u32) -> u32 {
        self.registers.get(&address).copied().unwrap_or(0)
    }
    fn destination(&self) -> SocketAddr {
        SocketAddr::from((
            Ipv4Addr::from(self.reg(STREAM_ADDRESS)),
            self.reg(STREAM_PORT) as u16,
        ))
    }
}
/// Device memory. Bootstrap registers hold the same values through register
/// and memory commands, as on a camera.
struct Device {
    bootstrap: Arc<Vec<u8>>,
    xml: Arc<Vec<u8>>,
    source: Arc<Mutex<Camera>>,
    commands: AcquisitionCommands,
    acquisition: Arc<AtomicBool>,
    stream: Arc<UdpSocket>,
}
impl Device {
    fn read(&self, state: &State, address: u64, length: usize) -> Result<Vec<u8>> {
        let mut bytes = read_memory(address, length, &self.bootstrap, &self.xml, &self.source)?;
        overlay(&state.registers, address, &mut bytes);
        Ok(bytes)
    }
    fn write(&self, state: &mut State, peer: SocketAddr, address: u64, data: &[u8]) -> Result<()> {
        if address >= SOURCE_BASE {
            ensure!(
                state.controller == Some(peer),
                Status(0x8006, "control privilege required")
            );
            return write_memory(
                address,
                data,
                &self.source,
                &self.commands,
                &self.acquisition,
            );
        }
        ensure!(
            address.is_multiple_of(4) && data.len().is_multiple_of(4),
            Status(0x8005, "unaligned register write")
        );
        for (address, word) in (address as u32..).step_by(4).zip(data.as_chunks::<4>().0) {
            self.write_register(state, peer, address, u32::from_be_bytes(*word))?;
        }
        Ok(())
    }
    fn write_register(
        &self,
        state: &mut State,
        peer: SocketAddr,
        address: u32,
        value: u32,
    ) -> Result<()> {
        ensure!(
            address.is_multiple_of(4),
            Status(0x8005, "unaligned register write")
        );
        if address == PRIVILEGE {
            ensure!(
                value <= 2 && (state.controller.is_none() || state.controller == Some(peer)),
                Status(0x8006, "camera already controlled")
            );
            state.heartbeat = Instant::now();
            if value == 0 {
                return self.release(state);
            }
            state.controller = Some(peer);
            state.registers.insert(PRIVILEGE, value);
            return Ok(());
        }
        ensure!(
            state.controller == Some(peer),
            Status(0x8006, "control privilege required")
        );
        match address {
            HEARTBEAT => ensure!((500..=60000).contains(&value), "heartbeat range"),
            STREAM_PORT => ensure!(value <= u16::MAX as u32, "invalid stream port"),
            STREAM_ADDRESS => ensure!(
                !Ipv4Addr::from(value).is_multicast() && value != u32::MAX,
                "invalid stream destination"
            ),
            STREAM_SIZE => {
                let size = value & 0xffff;
                ensure!((576..=9000).contains(&size), "stream packet size range");
                state.registers.insert(address, value & 0x7fff_ffff);
                if value & 0x8000_0000 != 0 && state.reg(STREAM_PORT) != 0 {
                    let _ = self
                        .stream
                        .send_to(&vec![0; size as usize - 28], state.destination());
                }
                return Ok(());
            }
            STREAM_DELAY => ensure!(value <= MAX_STREAM_DELAY, "stream packet delay range"),
            _ if (USER_NAME..USER_NAME + 16).contains(&address) => {}
            _ if (address as usize) < self.bootstrap.len() => {
                anyhow::bail!(Status(0x8004, "read-only bootstrap register"))
            }
            _ => {
                return write_memory(
                    address as u64,
                    &value.to_be_bytes(),
                    &self.source,
                    &self.commands,
                    &self.acquisition,
                );
            }
        }
        state.registers.insert(address, value);
        Ok(())
    }
    fn release(&self, state: &mut State) -> Result<()> {
        state.controller = None;
        state.registers.insert(PRIVILEGE, 0);
        self.acquisition.store(false, Ordering::Relaxed);
        self.source.lock().unwrap_or_else(|e| e.into_inner()).stop()
    }
}
/// Lays big-endian register values over the bootstrap bytes they cover.
fn overlay(registers: &BTreeMap<u32, u32>, address: u64, bytes: &mut [u8]) {
    for (&register, value) in registers {
        for (offset, byte) in (register as u64..).zip(value.to_be_bytes()) {
            if let Some(target) = offset
                .checked_sub(address)
                .and_then(|offset| usize::try_from(offset).ok())
                .and_then(|offset| bytes.get_mut(offset))
            {
                *target = byte;
            }
        }
    }
}
pub(crate) struct GigEServer {
    port: u16,
    threads: Vec<JoinHandle<()>>,
    shutdown: Arc<AtomicBool>,
}
impl GigEServer {
    pub(crate) fn start(
        options: &ServerOptions,
        port: u16,
        camera: Arc<Mutex<Camera>>,
        frames: Arc<LatestFrame>,
        acquisition: Arc<AtomicBool>,
        shutdown: Arc<AtomicBool>,
    ) -> Result<Self> {
        let control =
            UdpSocket::bind((options.bind, port)).context("binding virtual GigE control port")?;
        let port = control.local_addr()?.port();
        control.set_read_timeout(Some(Duration::from_millis(20)))?;
        let stream = Arc::new(
            UdpSocket::bind((options.bind, 0)).context("binding virtual GigE stream port")?,
        );
        let source_xml = camera
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .xml()
            .to_owned();
        let (xml, commands) = wire_xml(&source_xml)?;
        let xml_length = xml.len();
        let mut xml = xml.into_bytes();
        xml.resize(xml.len().next_multiple_of(4), 0);
        let xml = Arc::new(xml);
        let mut bootstrap = vec![0; 0x1000];
        bootstrap[0..4].copy_from_slice(&[0, 2, 0, 0]);
        bootstrap[4..8].copy_from_slice(&0x8000_0001u32.to_be_bytes());
        // A locally administered MAC, stable for each advertised IPv4 address.
        bootstrap[8..12].copy_from_slice(&0x0200u32.to_be_bytes());
        bootstrap[12..16].copy_from_slice(&u32::from(options.advertise).to_be_bytes());
        // A persistent (static) address; link-local addressing is always on.
        bootstrap[0x10..0x14].copy_from_slice(&5u32.to_be_bytes());
        bootstrap[0x14..0x18].copy_from_slice(&5u32.to_be_bytes());
        bootstrap[0x24..0x28].copy_from_slice(&options.advertise.octets());
        bootstrap[0x34..0x38].copy_from_slice(&[255, 255, 255, 0]);
        put_text(&mut bootstrap, 0x48, 32, "Capturefab");
        put_text(&mut bootstrap, 0x68, 32, "Virtual GigE camera");
        put_text(&mut bootstrap, 0x88, 32, env!("CARGO_PKG_VERSION"));
        put_text(
            &mut bootstrap,
            0xa8,
            48,
            "Test data replay; no authentication",
        );
        put_text(
            &mut bootstrap,
            0xd8,
            16,
            &format!("SIM{:08X}", u32::from(options.advertise)),
        );
        put_text(&mut bootstrap, USER_NAME as usize, 16, &options.name);
        put_text(
            &mut bootstrap,
            0x200,
            512,
            &format!("Local:Capturefab.xml;{XML_BASE:x};{xml_length:x}"),
        );
        bootstrap[0x600..0x604].copy_from_slice(&1u32.to_be_bytes());
        bootstrap[0x64c..0x650].copy_from_slice(&options.advertise.octets());
        bootstrap[0x65c..0x660].copy_from_slice(&[255, 255, 255, 0]);
        let bootstrap = Arc::new(bootstrap);
        let state = Arc::new(Mutex::new(State {
            registers: BTreeMap::from([
                (PRIVILEGE, 0),
                (HEARTBEAT, 3000),
                // Concatenation, WRITEMEM, PACKETRESEND, serial number and
                // user-defined name (arvgvcpprivate.h).
                (0x0934, 0xc000_0007),
                (0x093c, 0),
                (0x0940, 1_000_000_000),
                (0x0900, 0),
                (0x0904, 1),
                (0x0d1c, stream.local_addr()?.port() as u32),
                (STREAM_SIZE, 1500),
                (STREAM_PORT, 0),
                (STREAM_DELAY, 0),
                (STREAM_ADDRESS, 0),
            ]),
            controller: None,
            heartbeat: Instant::now(),
            cache: VecDeque::new(),
        }));
        let mut server = Self {
            port,
            threads: Vec::new(),
            shutdown: shutdown.clone(),
        };
        if let Some((sockets, netmask)) = discovery_sockets(options.bind, port) {
            let (replies, bootstrap, state, stopped) = (
                control.try_clone()?,
                bootstrap.clone(),
                state.clone(),
                shutdown.clone(),
            );
            let bind = u32::from(options.bind);
            server.threads.push(
                thread::Builder::new()
                    .name("capturefab-virtual-gvcp-discovery".into())
                    .spawn(move || {
                        let mut buffer = [0u8; 576];
                        while !stopped.load(Ordering::Relaxed) {
                            for socket in &sockets {
                                // Answer only the bound interface's subnet, as
                                // a camera on that link would.
                                if let Ok((length, SocketAddr::V4(peer))) =
                                    socket.recv_from(&mut buffer)
                                    && (u32::from(*peer.ip()) ^ bind) & netmask == 0
                                    && let Some((0x0002, id, _)) = command(&buffer[..length])
                                {
                                    let mut data = bootstrap[..0xf8].to_vec();
                                    let state = state.lock().unwrap_or_else(|e| e.into_inner());
                                    overlay(&state.registers, 0, &mut data);
                                    let _ = replies.send_to(&ack(0, 0x0002, id, &data), peer);
                                }
                            }
                        }
                    })
                    .context("starting virtual GigE discovery")?,
            );
        }
        let device = Device {
            bootstrap,
            xml,
            source: camera,
            commands,
            acquisition: acquisition.clone(),
            stream: stream.clone(),
        };
        let (state_control, stopped) = (state.clone(), shutdown.clone());
        server.threads.push(
            thread::Builder::new()
                .name("capturefab-virtual-gvcp".into())
                .spawn(move || {
                    let mut buffer = [0u8; 8192];
                    while !stopped.load(Ordering::Relaxed) {
                        {
                            let mut state = state_control.lock().unwrap_or_else(|e| e.into_inner());
                            if state.controller.is_some()
                                && state.heartbeat.elapsed()
                                    > Duration::from_millis(state.reg(HEARTBEAT).max(500) as u64)
                            {
                                let _ = device.release(&mut state);
                            }
                        }
                        let Ok((length, peer)) = control.recv_from(&mut buffer) else {
                            continue;
                        };
                        let Some((command, id, payload)) = command(&buffer[..length]) else {
                            continue;
                        };
                        let mut state = state_control.lock().unwrap_or_else(|e| e.into_inner());
                        if state.controller == Some(peer) {
                            state.heartbeat = Instant::now();
                        }
                        let mut written = None;
                        let response: Result<Vec<u8>> = (|| match command {
                            0x0002 => device.read(&state, 0, 0xf8),
                            0x0080 => {
                                ensure!(
                                    !payload.is_empty() && payload.len().is_multiple_of(4),
                                    "invalid register read"
                                );
                                let mut data = Vec::new();
                                for address in payload
                                    .as_chunks::<4>()
                                    .0
                                    .iter()
                                    .map(|bytes| be32(bytes, 0))
                                {
                                    ensure!(
                                        address.is_multiple_of(4),
                                        Status(0x8005, "unaligned register read")
                                    );
                                    // READ_REGISTER is also a four-byte memory
                                    // access. GenApi applies the XML byte order.
                                    data.extend(device.read(&state, address as u64, 4)?);
                                }
                                Ok(data)
                            }
                            0x0082 => {
                                ensure!(
                                    !payload.is_empty() && payload.len().is_multiple_of(8),
                                    "invalid register write"
                                );
                                for (index, pair) in payload.as_chunks::<8>().0.iter().enumerate() {
                                    // A failed acknowledgement carries the
                                    // number of writes already applied.
                                    written = Some(index as u32);
                                    device.write_register(
                                        &mut state,
                                        peer,
                                        be32(pair, 0),
                                        be32(pair, 4),
                                    )?;
                                }
                                Ok((payload.len() as u32 / 8).to_be_bytes().to_vec())
                            }
                            0x0084 => {
                                ensure!(payload.len() == 8, "invalid memory read");
                                let address = be32(payload, 0);
                                let length = be16(payload, 6) as usize;
                                ensure!((1..=512).contains(&length), "memory read limit");
                                let mut result = address.to_be_bytes().to_vec();
                                result.extend(device.read(&state, address as u64, length)?);
                                Ok(result)
                            }
                            0x0086 => {
                                ensure!(
                                    payload.len() >= 5 && payload.len() <= 516,
                                    "invalid memory write"
                                );
                                device.write(
                                    &mut state,
                                    peer,
                                    be32(payload, 0) as u64,
                                    &payload[4..],
                                )?;
                                Ok(payload[..4].to_vec())
                            }
                            0x0040 => {
                                ensure!(
                                    payload.len() == 12 && state.controller == Some(peer),
                                    "invalid resend request"
                                );
                                let block = be16(payload, 2);
                                let first = be32(payload, 4) as usize;
                                let last = be32(payload, 8) as usize;
                                if let Some((_, cached)) =
                                    state.cache.iter().find(|(id, _)| *id == block)
                                {
                                    for packet in cached
                                        .iter()
                                        .take(last.saturating_add(1))
                                        .skip(first)
                                        .take(4096)
                                    {
                                        let _ = device.stream.send_to(packet, state.destination());
                                    }
                                }
                                Ok(Vec::new())
                            }
                            _ => anyhow::bail!(Status(0x8001, "unsupported GVCP command")),
                        })();
                        if command == 0x0040 {
                            continue;
                        }
                        let (status, data) = match response {
                            Ok(data) => (0, data),
                            Err(error) => (
                                error
                                    .downcast_ref::<Status>()
                                    .map_or(0x8002, |status| status.0),
                                written.map_or(Vec::new(), |index| index.to_be_bytes().to_vec()),
                            ),
                        };
                        let _ = control.send_to(&ack(status, command, id, &data), peer);
                    }
                })
                .context("starting virtual GigE control")?,
        );
        server.threads.push(
            thread::Builder::new()
                .name("capturefab-virtual-gvsp".into())
                .spawn(move || {
                    let mut sequence = 0;
                    let mut block = 0u16;
                    while !shutdown.load(Ordering::Relaxed) {
                        let Some((next, frame)) =
                            frames.wait_after(sequence, Duration::from_millis(100))
                        else {
                            continue;
                        };
                        sequence = next;
                        if !acquisition.load(Ordering::Relaxed) {
                            continue;
                        }
                        let (destination, size, delay) = {
                            let state = state.lock().unwrap_or_else(|e| e.into_inner());
                            if state.reg(STREAM_PORT) == 0 || state.reg(STREAM_ADDRESS) == 0 {
                                continue;
                            }
                            (
                                state.destination(),
                                (state.reg(STREAM_SIZE) & 0xffff) as usize,
                                // Timestamp ticks run at 1 GHz.
                                Duration::from_nanos(state.reg(STREAM_DELAY) as u64),
                            )
                        };
                        block = block.wrapping_add(1).max(1);
                        let packets =
                            Arc::new(frame_packets(block, &frame, size.saturating_sub(36).max(1)));
                        let mut owed = Duration::ZERO;
                        for packet in packets.iter() {
                            if shutdown.load(Ordering::Relaxed) {
                                break;
                            }
                            let _ = stream.send_to(packet, destination);
                            // Sleep the inter-packet delay in batches the
                            // scheduler can honor.
                            owed += delay;
                            if owed >= Duration::from_millis(1) {
                                thread::sleep(owed);
                                owed = Duration::ZERO;
                            }
                        }
                        if frame.data.len() <= MAX_CACHE_BYTES / 2 {
                            let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
                            if state.cache.len() == 2 {
                                state.cache.pop_front();
                            }
                            state.cache.push_back((block, packets));
                        }
                    }
                })
                .context("starting virtual GigE streaming")?,
        );
        Ok(server)
    }
    pub(crate) fn port(&self) -> u16 {
        self.port
    }
}
impl Drop for GigEServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}
/// Linux and macOS deliver broadcasts only to sockets bound to a broadcast or
/// wildcard address, so an interface-bound camera listens there for discovery
/// too, as the Aravis fake camera does. Windows delivers them to the control
/// socket. Returns the sockets and the interface netmask.
fn discovery_sockets(bind: Ipv4Addr, port: u16) -> Option<(Vec<UdpSocket>, u32)> {
    if cfg!(windows) || bind.is_loopback() || bind.is_unspecified() {
        return None;
    }
    let interface =
        if_addrs::get_if_addrs()
            .ok()?
            .into_iter()
            .find_map(|interface| match interface.addr {
                if_addrs::IfAddr::V4(v4) if v4.ip == bind => Some(v4),
                _ => None,
            })?;
    let reusable = |address: Ipv4Addr| -> std::io::Result<UdpSocket> {
        let socket = socket2::Socket::new(
            socket2::Domain::IPV4,
            socket2::Type::DGRAM,
            Some(socket2::Protocol::UDP),
        )?;
        socket.set_reuse_address(true)?;
        socket.bind(&SocketAddr::from((address, port)).into())?;
        Ok(socket.into())
    };
    let mut sockets = Vec::new();
    if let Ok(socket) = reusable(Ipv4Addr::BROADCAST) {
        sockets.push(socket);
        if let Some(subnet) = interface.broadcast.filter(|subnet| *subnet != bind)
            && let Ok(socket) = UdpSocket::bind((subnet, port))
        {
            sockets.push(socket);
        }
    } else if let Ok(socket) = reusable(Ipv4Addr::UNSPECIFIED) {
        // BSD kernels refuse to bind the limited broadcast address; a wildcard
        // socket receives both broadcasts while unicast stays with the control socket.
        sockets.push(socket);
    }
    for socket in &sockets {
        socket
            .set_read_timeout(Some(Duration::from_millis(20)))
            .ok()?;
    }
    (!sockets.is_empty()).then_some((sockets, u32::from(interface.netmask)))
}
/// A GVCP command header: command, request ID and payload.
fn command(packet: &[u8]) -> Option<(u16, u16, &[u8])> {
    (packet.len() >= 8 && packet[0] == 0x42 && be16(packet, 4) as usize == packet.len() - 8)
        .then(|| (be16(packet, 2), be16(packet, 6), &packet[8..]))
}
fn ack(status: u16, command: u16, id: u16, data: &[u8]) -> Vec<u8> {
    let mut reply = status.to_be_bytes().to_vec();
    reply.extend((command + 1).to_be_bytes());
    reply.extend((data.len() as u16).to_be_bytes());
    reply.extend(id.to_be_bytes());
    reply.extend(data);
    reply
}
fn be16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_be_bytes(bytes[offset..offset + 2].try_into().unwrap())
}
fn be32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap())
}
fn put_text(memory: &mut [u8], offset: usize, length: usize, value: &str) {
    let bytes = value.as_bytes();
    let size = bytes.len().min(length - 1);
    memory[offset..offset + size].copy_from_slice(&bytes[..size]);
}
fn read_memory(
    address: u64,
    length: usize,
    bootstrap: &[u8],
    xml: &[u8],
    source: &Mutex<Camera>,
) -> Result<Vec<u8>> {
    let end = address
        .checked_add(length as u64)
        .context("address overflow")?;
    if end <= bootstrap.len() as u64 {
        return Ok(bootstrap[address as usize..end as usize].to_vec());
    }
    if address >= XML_BASE && end <= XML_BASE + xml.len() as u64 {
        return Ok(xml[(address - XML_BASE) as usize..(end - XML_BASE) as usize].to_vec());
    }
    ensure!(
        address >= SOURCE_BASE && end <= XML_BASE,
        Status(0x8003, "invalid memory address")
    );
    source
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .read_memory(address - SOURCE_BASE, length)
}
fn write_memory(
    address: u64,
    data: &[u8],
    source: &Mutex<Camera>,
    commands: &AcquisitionCommands,
    acquisition: &AtomicBool,
) -> Result<()> {
    ensure!(
        address >= SOURCE_BASE && address.saturating_add(data.len() as u64) < XML_BASE,
        Status(0x8003, "invalid memory address")
    );
    let mut source = source.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((start, stop)) = commands.get(&address) {
        ensure!(data.len() == 4, "acquisition command must be four bytes");
        let value = u32::from_le_bytes(data.try_into().unwrap()) as u64;
        if value == *start {
            source.start()?;
            acquisition.store(true, Ordering::Relaxed);
        } else if value == *stop {
            acquisition.store(false, Ordering::Relaxed);
            source.stop()?;
        } else {
            anyhow::bail!("invalid acquisition command");
        }
    } else {
        source.write_memory(address - SOURCE_BASE, data)?;
    }
    Ok(())
}
fn number(text: &str) -> Result<u64> {
    let text = text.trim();
    Ok(if let Some(hex) = text.strip_prefix("0x") {
        u64::from_str_radix(hex, 16)?
    } else {
        text.parse()?
    })
}
fn wire_xml(source: &str) -> Result<(String, AcquisitionCommands)> {
    let doc = roxmltree::Document::parse(source)?;
    let mut edits = Vec::new();
    let mut commands = BTreeMap::new();
    for node in doc
        .descendants()
        .filter(|node| node.is_element() && node.tag_name().name() == "Address")
    {
        let address = number(node.text().unwrap_or(""))?;
        ensure!(
            address < XML_BASE - SOURCE_BASE,
            "source register address exceeds virtual camera memory"
        );
        edits.push((
            node.range(),
            format!("<Address>{}</Address>", SOURCE_BASE + address),
        ));
    }
    for command in doc
        .descendants()
        .filter(|node| node.is_element() && node.tag_name().name() == "Command")
    {
        let name = command.attribute("Name").unwrap_or("");
        if !["AcquisitionStart", "AcquisitionStop"].contains(&name) {
            continue;
        }
        let register = command
            .children()
            .find(|n| n.has_tag_name("pValue"))
            .and_then(|n| n.text())
            .context("acquisition command has no register")?;
        let register = doc
            .descendants()
            .find(|n| n.attribute("Name") == Some(register))
            .context("acquisition register missing")?;
        let address = number(
            register
                .children()
                .find(|n| n.has_tag_name("Address"))
                .and_then(|n| n.text())
                .context("acquisition register address missing")?,
        )? + SOURCE_BASE;
        let value = number(
            command
                .children()
                .find(|n| n.has_tag_name("CommandValue"))
                .and_then(|n| n.text())
                .context("acquisition command value missing")?,
        )?;
        let values = commands.entry(address).or_insert((1, 0));
        if name == "AcquisitionStart" {
            values.0 = value;
        } else {
            values.1 = value;
        }
    }
    for register in doc.descendants().filter(|n| {
        n.is_element()
            && matches!(
                n.tag_name().name(),
                "IntReg" | "FloatReg" | "StringReg" | "Register"
            )
    }) {
        if !register.children().any(|n| n.has_tag_name("pPort")) {
            let close = source[register.range()]
                .rfind("</")
                .context("register closing tag missing")?
                + register.range().start;
            edits.push((close..close, "<pPort>Device</pPort>".into()));
        }
    }
    if !doc
        .descendants()
        .any(|n| n.has_tag_name("Port") && n.attribute("Name") == Some("Device"))
    {
        let close = source
            .rfind("</RegisterDescription>")
            .context("camera XML root missing")?;
        edits.push((close..close, "<Port Name=\"Device\"/>".into()));
    }
    edits.sort_by_key(|(range, _)| std::cmp::Reverse(range.start));
    let mut result = source.to_owned();
    for (range, value) in edits {
        result.replace_range(range, &value);
    }
    Ok((result, commands))
}
fn packet(block: u16, id: usize, kind: u32, data: &[u8]) -> Vec<u8> {
    let mut packet = vec![0, 0];
    packet.extend(block.to_be_bytes());
    packet.extend(((kind << 24) | (id as u32)).to_be_bytes());
    packet.extend(data);
    packet
}
fn frame_packets(block: u16, frame: &Frame, chunk: usize) -> Vec<Vec<u8>> {
    let mut leader = vec![0, 0, 0, 1];
    leader.extend(frame.timestamp_ns.to_be_bytes());
    leader.extend(frame.pixel_format.to_be_bytes());
    leader.extend(frame.width.to_be_bytes());
    leader.extend(frame.height.to_be_bytes());
    leader.extend([0; 12]);
    let mut packets = vec![packet(block, 0, 1, &leader)];
    for payload in frame.data.chunks(chunk) {
        packets.push(packet(block, packets.len(), 3, payload));
    }
    packets.push(packet(
        block,
        packets.len(),
        2,
        &[1u32.to_be_bytes(), frame.height.to_be_bytes()].concat(),
    ));
    packets
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn remapped_xml_and_gvsp_stream_open_through_native_camera() {
        let ip = Ipv4Addr::LOCALHOST;
        let mut source = Camera::open("sim:0", &[], Duration::from_millis(100)).unwrap();
        source.set("Width", "64").unwrap();
        source.set("Height", "64").unwrap();
        let options = ServerOptions {
            bind: ip,
            advertise: ip,
            gige_port: Some(3956),
            rtsp_port: None,
            onvif_port: None,
            name: "GigE test".into(),
            fps: 30.0,
            bitrate: "2M".into(),
        };
        let server = super::super::VirtualCameraServer::start(source, options).unwrap();
        let mut camera = Camera::open(&format!("gige:{ip}"), &[], Duration::from_secs(2)).unwrap();
        assert_eq!(camera.get("Width").unwrap(), 64);
        camera.set("Gain", "3").unwrap();
        assert_eq!(camera.get("Gain").unwrap(), 3.0);
        camera.start().unwrap();
        let frame = camera.next_frame(Duration::from_secs(2)).unwrap();
        assert_eq!(
            (frame.width, frame.height, frame.data.len()),
            (64, 64, 4096)
        );
        camera.stop().unwrap();
        // Four-byte feature access exercises GVCP register transactions; Gain
        // above exercises the eight-byte READ/WRITE_MEMORY path.
        camera.set("Width", "96").unwrap();
        assert_eq!(camera.get("Width").unwrap(), 96);
        camera.start().unwrap();
        let frame = camera.next_frame(Duration::from_secs(2)).unwrap();
        assert_eq!(
            (frame.width, frame.height, frame.data.len()),
            (96, 64, 6144)
        );
        camera.stop().unwrap();
        drop(camera);
        let mut camera = Camera::open(&format!("gige:{ip}"), &[], Duration::from_secs(2)).unwrap();
        camera.start().unwrap();
        assert_eq!(camera.next_frame(Duration::from_secs(2)).unwrap().width, 96);
        drop(camera);
        // A discovered camera opens by the MAC-based ID discovery reports.
        let info = crate::transport::gige::probe(ip, Duration::from_secs(2)).unwrap();
        assert!(
            info.id
                .strip_prefix("gige:")
                .unwrap()
                .parse::<Ipv4Addr>()
                .is_err()
        );
        let id = info.id.clone();
        let camera = Camera::open(&id, &[info], Duration::from_secs(2)).unwrap();
        assert_eq!(camera.info.id, id);
        drop(camera);
        drop(server);
    }

    fn loopback_camera() -> (super::super::VirtualCameraServer, SocketAddr) {
        let ip = Ipv4Addr::LOCALHOST;
        let mut source = Camera::open("sim:0", &[], Duration::from_millis(100)).unwrap();
        source.set("Width", "64").unwrap();
        source.set("Height", "64").unwrap();
        let options = ServerOptions {
            bind: ip,
            advertise: ip,
            gige_port: Some(0),
            rtsp_port: None,
            onvif_port: None,
            name: "GVCP test".into(),
            fps: 30.0,
            bitrate: "2M".into(),
        };
        let server = super::super::VirtualCameraServer::start(source, options).unwrap();
        let port = server.endpoints()["gige"]["port"].as_u64().unwrap() as u16;
        (server, SocketAddr::from((ip, port)))
    }
    fn client() -> UdpSocket {
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        socket
    }
    /// Sends one GVCP command; returns the acknowledgement status and payload.
    fn gvcp(
        socket: &UdpSocket,
        camera: SocketAddr,
        command: u16,
        id: u16,
        payload: &[u8],
    ) -> (u16, Vec<u8>) {
        let mut request = vec![0x42, 0x01];
        request.extend(command.to_be_bytes());
        request.extend((payload.len() as u16).to_be_bytes());
        request.extend(id.to_be_bytes());
        request.extend(payload);
        socket.send_to(&request, camera).unwrap();
        let mut reply = [0u8; 1024];
        let (length, from) = socket.recv_from(&mut reply).unwrap();
        assert_eq!(from, camera);
        assert_eq!(
            (be16(&reply, 2), be16(&reply, 4) as usize, be16(&reply, 6)),
            (command + 1, length - 8, id)
        );
        (be16(&reply, 0), reply[8..length].to_vec())
    }

    #[test]
    fn bootstrap_registers_answer_register_and_memory_commands_alike() {
        let (server, camera) = loopback_camera();
        let (controller, other) = (client(), client());
        // Interfaces, GVCP capability and current IP configuration in one
        // concatenated READREG.
        assert_eq!(
            gvcp(
                &controller,
                camera,
                0x0080,
                1,
                &[0, 0, 6, 0, 0, 0, 9, 0x34, 0, 0, 0, 0x14]
            ),
            (0, vec![0, 0, 0, 1, 0xc0, 0, 0, 7, 0, 0, 0, 5])
        );
        // The 64-bit timestamp tick frequency through READMEM: 1 GHz.
        assert_eq!(
            gvcp(&controller, camera, 0x0084, 2, &[0, 0, 9, 0x3c, 0, 0, 0, 8]),
            (0, vec![0, 0, 9, 0x3c, 0, 0, 0, 0, 0x3b, 0x9a, 0xca, 0x00])
        );
        assert_eq!(
            gvcp(&controller, camera, 0x0082, 3, &[0, 0, 0x0a, 0, 0, 0, 0, 2]),
            (0, vec![0, 0, 0, 1])
        );
        assert_eq!(
            gvcp(&other, camera, 0x0082, 4, &[0, 0, 0x0a, 0, 0, 0, 0, 2]),
            (0x8006, vec![0, 0, 0, 0])
        );
        // SCPD written as memory reads back as a register, and SCP written as
        // a register reads back as memory.
        assert_eq!(
            gvcp(
                &controller,
                camera,
                0x0086,
                5,
                &[0, 0, 0x0d, 8, 0, 0, 3, 0xe8]
            ),
            (0, vec![0, 0, 0x0d, 8])
        );
        assert_eq!(
            gvcp(&controller, camera, 0x0080, 6, &[0, 0, 0x0d, 8]),
            (0, vec![0, 0, 3, 0xe8])
        );
        assert_eq!(
            gvcp(
                &controller,
                camera,
                0x0082,
                7,
                &[0, 0, 0x0d, 0, 0, 0, 0x12, 0x34]
            )
            .0,
            0
        );
        assert_eq!(
            gvcp(&controller, camera, 0x0084, 8, &[0, 0, 0x0d, 0, 0, 0, 0, 4]),
            (0, vec![0, 0, 0x0d, 0, 0, 0, 0x12, 0x34])
        );
        let name = *b"Line 3\0\0\0\0\0\0\0\0\0\0";
        assert_eq!(
            gvcp(
                &controller,
                camera,
                0x0086,
                9,
                &[&[0, 0, 0, 0xe8][..], &name].concat()
            )
            .0,
            0
        );
        assert_eq!(gvcp(&other, camera, 0x0002, 10, &[]).1[0xe8..0xf8], name);
        // A failed multi-register write reports how many writes it applied.
        assert_eq!(
            gvcp(
                &controller,
                camera,
                0x0082,
                11,
                &[
                    0, 0, 0x0d, 8, 0, 0, 0, 5, 0, 0, 0, 0x24, 0, 0, 0, 1, 0, 0, 0x0d, 8, 0, 0, 0, 6
                ]
            ),
            (0x8004, vec![0, 0, 0, 1])
        );
        assert_eq!(
            gvcp(&controller, camera, 0x0080, 12, &[0, 0, 0x0d, 8]),
            (0, vec![0, 0, 0, 5])
        );
        for (command, payload, status) in [
            (0x0082, vec![0, 0, 0x0d, 6, 0, 0, 0, 0], 0x8005),
            (0x0080, vec![0, 0, 0, 2], 0x8005),
            (0x0084, vec![0, 0x20, 0, 0, 0, 0, 0, 4], 0x8003),
            (0x00f0, vec![], 0x8001),
        ] {
            assert_eq!(
                gvcp(&controller, camera, command, 13, &payload).0,
                status,
                "{command:04x}"
            );
        }
        assert_eq!(
            gvcp(&other, camera, 0x0086, 14, &[0, 1, 1, 0, 0x80, 0, 0, 0]).0,
            0x8006
        );
        drop(server);
    }

    #[test]
    fn interface_bound_camera_answers_broadcast_discovery() {
        // Needs a broadcast-capable LAN interface; hosts without one have
        // nothing to test.
        let Some((ip, subnet)) =
            if_addrs::get_if_addrs()
                .unwrap()
                .into_iter()
                .find_map(|interface| match interface.addr {
                    if_addrs::IfAddr::V4(v4) if !v4.ip.is_loopback() && !v4.ip.is_link_local() => {
                        v4.broadcast.map(|broadcast| (v4.ip, broadcast))
                    }
                    _ => None,
                })
        else {
            return;
        };
        let source = Camera::open("sim:0", &[], Duration::from_millis(100)).unwrap();
        let options = ServerOptions {
            bind: ip,
            advertise: ip,
            gige_port: Some(0),
            rtsp_port: None,
            onvif_port: None,
            name: "Broadcast test".into(),
            fps: 30.0,
            bitrate: "2M".into(),
        };
        let server = super::super::VirtualCameraServer::start(source, options).unwrap();
        let port = server.endpoints()["gige"]["port"].as_u64().unwrap() as u16;
        let client = UdpSocket::bind((ip, 0)).unwrap();
        client.set_broadcast(true).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        for (id, destination) in [(0x1234u16, subnet), (0x1235, Ipv4Addr::BROADCAST)] {
            // DISCOVERY_CMD with the acknowledge-required and broadcast-allowed
            // flags, as Aravis sends it (arvgvcp.c).
            let request = [
                0x42,
                0x11,
                0x00,
                0x02,
                0x00,
                0x00,
                (id >> 8) as u8,
                id as u8,
            ];
            if client.send_to(&request, (destination, port)).is_err() {
                return;
            }
            let mut reply = [0u8; 1024];
            let (length, from) = client.recv_from(&mut reply).unwrap();
            assert_eq!(from, SocketAddr::from((ip, port)), "{destination}");
            assert_eq!(length, 8 + 0xf8);
            assert_eq!(
                reply[..8],
                [0, 0, 0x00, 0x03, 0x00, 0xf8, request[6], request[7]]
            );
            assert_eq!(reply[8 + 0x24..8 + 0x28], ip.octets());
        }
        drop(server);
    }
}
