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
const STREAM_ADDRESS: u32 = 0x0d18;
const MAX_CACHE_BYTES: usize = 32 * 1024 * 1024;
type AcquisitionCommands = BTreeMap<u64, (u64, u64)>;

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
        bootstrap[0x10..0x14].copy_from_slice(&4u32.to_be_bytes());
        bootstrap[0x14..0x18].copy_from_slice(&4u32.to_be_bytes());
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
        put_text(&mut bootstrap, 0xe8, 16, &options.name);
        put_text(
            &mut bootstrap,
            0x200,
            512,
            &format!("Local:Capturefab.xml;{XML_BASE:x};{xml_length:x}"),
        );
        let bootstrap = Arc::new(bootstrap);
        let state = Arc::new(Mutex::new(State {
            registers: BTreeMap::from([
                (PRIVILEGE, 0),
                (HEARTBEAT, 3000),
                (0x0934, 0x4),
                (0x093c, 0),
                (0x0940, 1_000_000_000),
                (0x0900, 0),
                (0x0904, 1),
                (0x0d1c, stream.local_addr()?.port() as u32),
                (STREAM_SIZE, 1500),
                (STREAM_PORT, 0),
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
        let (state_control, source, stopped, active, packets) = (
            state.clone(),
            camera,
            shutdown.clone(),
            acquisition.clone(),
            stream.clone(),
        );
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
                                state.controller = None;
                                state.registers.insert(PRIVILEGE, 0);
                                active.store(false, Ordering::Relaxed);
                                let _ = source.lock().unwrap_or_else(|e| e.into_inner()).stop();
                            }
                        }
                        let Ok((length, peer)) = control.recv_from(&mut buffer) else {
                            continue;
                        };
                        if length < 8
                            || buffer[0] != 0x42
                            || be16(&buffer, 4) as usize != length - 8
                        {
                            continue;
                        }
                        let command = be16(&buffer, 2);
                        let id = be16(&buffer, 6);
                        let payload = &buffer[8..length];
                        let mut state = state_control.lock().unwrap_or_else(|e| e.into_inner());
                        if state.controller == Some(peer) {
                            state.heartbeat = Instant::now();
                        }
                        let response: Result<Vec<u8>> = (|| match command {
                            0x0002 => Ok(bootstrap[..0xf8].to_vec()),
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
                                    ensure!(address.is_multiple_of(4), "unaligned register read");
                                    if state.registers.contains_key(&address) {
                                        data.extend(state.reg(address).to_be_bytes());
                                    } else {
                                        let bytes = read_memory(
                                            address as u64,
                                            4,
                                            &bootstrap,
                                            &xml,
                                            &source,
                                        )?;
                                        // READ_REGISTER is also a four-byte memory
                                        // access. GenApi applies the XML byte order.
                                        data.extend(bytes);
                                    }
                                }
                                Ok(data)
                            }
                            0x0082 => {
                                ensure!(
                                    !payload.is_empty() && payload.len().is_multiple_of(8),
                                    "invalid register write"
                                );
                                for pair in payload.as_chunks::<8>().0 {
                                    let (address, value) = (be32(pair, 0), be32(pair, 4));
                                    ensure!(address.is_multiple_of(4), "unaligned register write");
                                    if address == PRIVILEGE {
                                        ensure!(
                                            value <= 2
                                                && (state.controller.is_none()
                                                    || state.controller == Some(peer)),
                                            "camera already controlled"
                                        );
                                        state.controller = (value != 0).then_some(peer);
                                        state.registers.insert(PRIVILEGE, value);
                                        state.heartbeat = Instant::now();
                                        if value == 0 {
                                            active.store(false, Ordering::Relaxed);
                                            source
                                                .lock()
                                                .unwrap_or_else(|e| e.into_inner())
                                                .stop()?;
                                        }
                                    } else {
                                        ensure!(
                                            state.controller == Some(peer),
                                            "control privilege required"
                                        );
                                        match address {
                                            HEARTBEAT => {
                                                ensure!(
                                                    (500..=60000).contains(&value),
                                                    "heartbeat range"
                                                );
                                                state.registers.insert(address, value);
                                            }
                                            STREAM_PORT => {
                                                ensure!(
                                                    value <= u16::MAX as u32,
                                                    "invalid stream port"
                                                );
                                                state.registers.insert(address, value);
                                            }
                                            STREAM_ADDRESS => {
                                                ensure!(
                                                    !Ipv4Addr::from(value).is_multicast()
                                                        && value != u32::MAX,
                                                    "invalid stream destination"
                                                );
                                                state.registers.insert(address, value);
                                            }
                                            STREAM_SIZE => {
                                                let size = value & 0xffff;
                                                ensure!(
                                                    (576..=9000).contains(&size),
                                                    "stream packet size range"
                                                );
                                                state
                                                    .registers
                                                    .insert(address, value & 0x7fff_ffff);
                                                if value & 0x8000_0000 != 0
                                                    && state.reg(STREAM_PORT) != 0
                                                {
                                                    let _ = packets.send_to(
                                                        &vec![0; size as usize - 28],
                                                        state.destination(),
                                                    );
                                                }
                                            }
                                            _ => write_memory(
                                                address as u64,
                                                &value.to_be_bytes(),
                                                &source,
                                                &commands,
                                                &active,
                                            )?,
                                        }
                                    }
                                }
                                Ok((payload.len() as u32 / 8).to_be_bytes().to_vec())
                            }
                            0x0084 => {
                                ensure!(payload.len() == 8, "invalid memory read");
                                let address = be32(payload, 0);
                                let length = be16(payload, 6) as usize;
                                ensure!((1..=512).contains(&length), "memory read limit");
                                let mut result = address.to_be_bytes().to_vec();
                                result.extend(read_memory(
                                    address as u64,
                                    length,
                                    &bootstrap,
                                    &xml,
                                    &source,
                                )?);
                                Ok(result)
                            }
                            0x0086 => {
                                ensure!(
                                    state.controller == Some(peer),
                                    "control privilege required"
                                );
                                ensure!(
                                    payload.len() >= 5 && payload.len() <= 516,
                                    "invalid memory write"
                                );
                                write_memory(
                                    be32(payload, 0) as u64,
                                    &payload[4..],
                                    &source,
                                    &commands,
                                    &active,
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
                                        let _ = packets.send_to(packet, state.destination());
                                    }
                                }
                                Ok(Vec::new())
                            }
                            _ => anyhow::bail!("unsupported GVCP command"),
                        })();
                        if command == 0x0040 {
                            continue;
                        }
                        let (status, data) = match response {
                            Ok(data) => (0u16, data),
                            Err(error) => {
                                let message = error.to_string();
                                let status = if message.contains("privilege")
                                    || message.contains("controlled")
                                {
                                    0x8006
                                } else if command != 0x0002
                                    && ![0x0080, 0x0082, 0x0084, 0x0086].contains(&command)
                                {
                                    0x8001
                                } else {
                                    0x8002
                                };
                                (status, Vec::new())
                            }
                        };
                        let mut reply = status.to_be_bytes().to_vec();
                        reply.extend((command + 1).to_be_bytes());
                        reply.extend((data.len() as u16).to_be_bytes());
                        reply.extend(id.to_be_bytes());
                        reply.extend(data);
                        let _ = control.send_to(&reply, peer);
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
                        let (destination, size) = {
                            let state = state.lock().unwrap_or_else(|e| e.into_inner());
                            if state.reg(STREAM_PORT) == 0 || state.reg(STREAM_ADDRESS) == 0 {
                                continue;
                            }
                            (
                                state.destination(),
                                (state.reg(STREAM_SIZE) & 0xffff) as usize,
                            )
                        };
                        block = block.wrapping_add(1).max(1);
                        let packets =
                            Arc::new(frame_packets(block, &frame, size.saturating_sub(36).max(1)));
                        for packet in packets.iter() {
                            if shutdown.load(Ordering::Relaxed) {
                                break;
                            }
                            let _ = stream.send_to(packet, destination);
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
        "invalid memory address"
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
        "invalid memory address"
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
        drop(server);
    }
}
