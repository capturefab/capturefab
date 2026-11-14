//! Synthetic native GVCP/GVSP peer, not a physical camera qualification.
use super::*;
use crate::{camera::Camera, network::action};
use bootstrap::{Channel, Datagram, Interface, Socket};
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::atomic::{AtomicBool, Ordering},
};

const KEY: u32 = 0x1_1000;
const GROUP: u32 = 0x1_1004;
const MASK: u32 = 0x1_1008;
const ACQUISITION: u32 = 0x1_100c;
const SOURCE: u32 = 0x1_1010;
const TRIGGER: u32 = 0x1_1014;
pub(crate) const PIXELS: [u8; 8] = [0x10, 0x21, 0x32, 0x43, 0x54, 0x65, 0x76, 0x87];

const XML: &str = r#"<RegisterDescription ModelName="ActionFixture" VendorName="Capturefab">
<Integer Name="Width"><Value>4</Value><ImposedAccessMode>RO</ImposedAccessMode></Integer>
<Integer Name="Height"><Value>2</Value><ImposedAccessMode>RO</ImposedAccessMode></Integer>
<Integer Name="PayloadSize"><Value>8</Value><ImposedAccessMode>RO</ImposedAccessMode></Integer>
<Enumeration Name="PixelFormat"><EnumEntry Name="Mono8"><Value>0x01080001</Value></EnumEntry><Value>0x01080001</Value><ImposedAccessMode>RO</ImposedAccessMode></Enumeration>
<Enumeration Name="AcquisitionMode"><EnumEntry Name="Continuous"><Value>0</Value></EnumEntry><Value>0</Value><ImposedAccessMode>RO</ImposedAccessMode></Enumeration>
<Enumeration Name="TriggerSelector"><EnumEntry Name="FrameStart"><Value>0</Value></EnumEntry><Value>0</Value><ImposedAccessMode>RO</ImposedAccessMode></Enumeration>
<Enumeration Name="TriggerSource"><EnumEntry Name="Action1"><Value>1</Value></EnumEntry><pValue>SourceReg</pValue></Enumeration>
<IntReg Name="SourceReg"><Address>0x11010</Address><Length>4</Length><AccessMode>RW</AccessMode><Sign>Unsigned</Sign><Endianess>BigEndian</Endianess></IntReg>
<Enumeration Name="TriggerMode"><EnumEntry Name="Off"><Value>0</Value></EnumEntry><EnumEntry Name="On"><Value>1</Value></EnumEntry><pValue>TriggerReg</pValue></Enumeration>
<IntReg Name="TriggerReg"><Address>0x11014</Address><Length>4</Length><AccessMode>RW</AccessMode><Sign>Unsigned</Sign><Endianess>BigEndian</Endianess></IntReg>
<Integer Name="ActionDeviceKey"><pValue>KeyReg</pValue><Min>0</Min><Max>4294967295</Max></Integer>
<IntReg Name="KeyReg"><Address>0x11000</Address><Length>4</Length><AccessMode>WO</AccessMode><Sign>Unsigned</Sign><Endianess>BigEndian</Endianess></IntReg>
<Integer Name="ActionGroupKey"><pValue>GroupReg</pValue><Min>0</Min><Max>4294967295</Max></Integer>
<IntReg Name="GroupReg"><Address>0x11004</Address><Length>4</Length><AccessMode>RW</AccessMode><Sign>Unsigned</Sign><Endianess>BigEndian</Endianess></IntReg>
<Integer Name="ActionGroupMask"><pValue>MaskReg</pValue><Min>0</Min><Max>4294967295</Max></Integer>
<IntReg Name="MaskReg"><Address>0x11008</Address><Length>4</Length><AccessMode>RW</AccessMode><Sign>Unsigned</Sign><Endianess>BigEndian</Endianess></IntReg>
<Command Name="AcquisitionStart"><pValue>AcquisitionReg</pValue><CommandValue>1</CommandValue></Command>
<Command Name="AcquisitionStop"><pValue>AcquisitionReg</pValue><CommandValue>0</CommandValue></Command>
<IntReg Name="AcquisitionReg"><Address>0x1100c</Address><Length>4</Length><AccessMode>WO</AccessMode><Sign>Unsigned</Sign><Endianess>BigEndian</Endianess></IntReg>
</RegisterDescription>"#;

#[derive(Clone, Default, Debug)]
pub(crate) struct Snapshot {
    pub acquisition_started: bool,
    pub starts: usize,
    pub actions: Vec<Vec<u8>>,
    pub device_ticks: u64,
    pub pending_action: Option<u64>,
    pub key_reads: usize,
    pub key_writes: usize,
    pub frame_count: usize,
    pub claims: usize,
    pub releases: usize,
    pub events: Vec<&'static str>,
}

struct State {
    registers: HashMap<u32, u32>,
    owner: Option<SocketAddrV4>,
    emit_frames: bool,
    snapshot: Snapshot,
}
impl State {
    fn word(&self, address: u32) -> u32 {
        self.registers.get(&address).copied().unwrap_or_default()
    }
    fn put(&mut self, address: u32, value: u32) {
        self.registers.insert(address, value);
        if address == KEY {
            self.snapshot.key_writes += 1;
        }
        if address == ACQUISITION {
            self.snapshot.acquisition_started = value == 1;
            if value == 1 {
                self.snapshot.starts += 1;
                self.snapshot.events.push("acquisition_start");
            }
        }
    }
    fn frame_destination(&mut self) -> Option<SocketAddrV4> {
        if !self.emit_frames || !self.snapshot.acquisition_started {
            return None;
        }
        let destination = self.destination()?;
        self.snapshot.frame_count += 1;
        self.snapshot.events.push("frame");
        Some(destination)
    }
    fn due_frame(&mut self) -> Option<SocketAddrV4> {
        if self
            .snapshot
            .pending_action
            .is_some_and(|target| target <= self.snapshot.device_ticks)
        {
            self.snapshot.pending_action = None;
            return self.frame_destination();
        }
        None
    }
    fn destination(&self) -> Option<SocketAddrV4> {
        let port = u16::try_from(self.word(STREAM_PORT))
            .ok()
            .filter(|port| *port != 0)?;
        Some(SocketAddrV4::new(
            Ipv4Addr::from(self.word(STREAM_ADDRESS)),
            port,
        ))
    }
}

fn u16_at(data: &[u8], offset: usize) -> u16 {
    u16::from_be_bytes(data[offset..offset + 2].try_into().unwrap())
}
fn u32_at(data: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap())
}
fn reply(command: u16, id: u16, status: u16, data: &[u8]) -> Vec<u8> {
    let mut packet = status.to_be_bytes().to_vec();
    packet.extend_from_slice(&command.to_be_bytes());
    packet.extend_from_slice(&u16::try_from(data.len()).unwrap().to_be_bytes());
    packet.extend_from_slice(&id.to_be_bytes());
    packet.extend_from_slice(data);
    packet
}
fn frame_packets() -> [Vec<u8>; 3] {
    // Independent fixed standard IMAGE leader, data and height trailer.
    let mut leader = vec![0, 0, 0, 42, 1, 0, 0, 0, 0, 0, 0, 1];
    leader.extend_from_slice(&1234_u64.to_be_bytes());
    leader.extend_from_slice(&0x0108_0001_u32.to_be_bytes());
    leader.extend_from_slice(&4_u32.to_be_bytes());
    leader.extend_from_slice(&2_u32.to_be_bytes());
    leader.extend_from_slice(&[0; 12]);
    let mut data = vec![0, 0, 0, 42, 3, 0, 0, 1];
    data.extend_from_slice(&PIXELS);
    let trailer = vec![0, 0, 0, 42, 2, 0, 0, 2, 0, 0, 0, 1, 0, 0, 0, 2];
    [leader, data, trailer]
}

pub(crate) struct Harness {
    address: SocketAddrV4,
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}
impl Harness {
    pub(crate) fn new() -> Self {
        let control = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        control
            .set_read_timeout(Some(Duration::from_millis(5)))
            .unwrap();
        let stream = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        control
            .set_write_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let SocketAddr::V4(address) = control.local_addr().unwrap() else {
            unreachable!()
        };
        let registers = HashMap::from([
            (HEARTBEAT_TIMEOUT, 3000),
            (DEVICE_MODE, 0x8000_0000),
            (GVCP_CAPABILITY, OWNER_ENDPOINT | 2),
            (STREAM_PACKET_SIZE, 1500),
            (TIMESTAMP_LOW, 1000),
            (SOURCE, 1),
            (TRIGGER, 1),
        ]);
        let state = Arc::new(Mutex::new(State {
            registers,
            owner: None,
            emit_frames: true,
            snapshot: Snapshot {
                device_ticks: 1000,
                ..Snapshot::default()
            },
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let (shared, stopped) = (state.clone(), stop.clone());
        let thread = thread::spawn(move || {
            let mut buffer = [0_u8; 4096];
            while !stopped.load(Ordering::Relaxed) {
                // Only an explicit fixture clock advance releases future actions.
                // No wall-clock scheduling or physical timing claim is involved.
                let destination = shared.lock().unwrap().due_frame();
                if let Some(destination) = destination {
                    for packet in frame_packets() {
                        stream.send_to(&packet, destination).unwrap();
                    }
                }
                let (length, peer) = match control.recv_from(&mut buffer) {
                    Ok(value) => value,
                    Err(error)
                        if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
                    {
                        continue;
                    }
                    Err(error) => panic!("synthetic control receive: {error}"),
                };
                let packet = &buffer[..length];
                assert!(packet.len() >= 8);
                assert_eq!(packet[0], 0x42);
                assert_eq!(usize::from(u16_at(packet, 4)), length - 8);
                let (command, id) = (u16_at(packet, 2), u16_at(packet, 6));
                let SocketAddr::V4(peer) = peer else {
                    unreachable!()
                };
                let mut state = shared.lock().unwrap();
                let mut outgoing = Vec::new();
                let response = match command {
                    0x0100 => {
                        let scheduled = packet[1] & 0x80 != 0;
                        assert_eq!(length, if scheduled { 28 } else { 20 });
                        assert!(matches!(packet[1], 0 | 1 | 0x80 | 0x81));
                        state.snapshot.actions.push(packet.to_vec());
                        state.snapshot.events.push("action");
                        let matches = state.owner.is_some()
                            && state.snapshot.acquisition_started
                            && state.word(SOURCE) == 1
                            && state.word(TRIGGER) == 1
                            && u32_at(packet, 8) == state.word(KEY)
                            && u32_at(packet, 12) == state.word(GROUP)
                            && u32_at(packet, 16) & state.word(MASK) != 0;
                        let mut status = 0;
                        if matches {
                            let target = scheduled
                                .then(|| u64::from_be_bytes(packet[20..28].try_into().unwrap()));
                            if let Some(target) =
                                target.filter(|target| *target > state.snapshot.device_ticks)
                            {
                                if state.snapshot.pending_action.is_some() {
                                    status = 0x8015;
                                } else {
                                    state.snapshot.pending_action = Some(target);
                                    state.snapshot.events.push("scheduled_queued");
                                }
                            } else if let Some(destination) = state.frame_destination() {
                                outgoing.extend(
                                    frame_packets()
                                        .into_iter()
                                        .map(|packet| (packet, destination)),
                                );
                            }
                        }
                        (matches && packet[1] & 1 != 0).then(|| reply(0x0101, id, status, &[]))
                    }
                    0x0080 => {
                        assert!((length - 8).is_multiple_of(4));
                        let addresses: Vec<_> = packet[8..]
                            .as_chunks::<4>()
                            .0
                            .iter()
                            .map(|raw| u32::from_be_bytes(*raw))
                            .collect();
                        if addresses.contains(&KEY) {
                            state.snapshot.key_reads += 1;
                            Some(reply(0x0081, id, 0x8006, &[]))
                        } else {
                            let data: Vec<_> = addresses
                                .into_iter()
                                .flat_map(|a| state.word(a).to_be_bytes())
                                .collect();
                            Some(reply(0x0081, id, 0, &data))
                        }
                    }
                    0x0082 => {
                        assert_eq!(length, 16);
                        let (address, value) = (u32_at(packet, 8), u32_at(packet, 12));
                        if address == CONTROL_PRIVILEGE && value == 2 && state.owner.is_none() {
                            state.owner = Some(peer);
                            state.snapshot.claims += 1;
                            state.registers.insert(PRIMARY_PORT, u32::from(peer.port()));
                            state.registers.insert(PRIMARY_IP, u32::from(*peer.ip()));
                        }
                        if state.owner == Some(peer) {
                            if address == STREAM_PACKET_SIZE && value & SCPS_FIRE != 0 {
                                let destination = state.destination().unwrap();
                                outgoing.push((
                                    vec![0; usize::try_from(value & 0xffff).unwrap() - 28],
                                    destination,
                                ));
                            }
                            state.put(
                                address,
                                value
                                    & if address == STREAM_PACKET_SIZE {
                                        !SCPS_FIRE
                                    } else {
                                        u32::MAX
                                    },
                            );
                            if address == CONTROL_PRIVILEGE && value == 0 {
                                state.owner = None;
                                state.snapshot.releases += 1;
                                state.registers.insert(PRIMARY_PORT, 0);
                                state.registers.insert(PRIMARY_IP, 0);
                            }
                            Some(reply(0x0083, id, 0, &1_u32.to_be_bytes()))
                        } else {
                            Some(reply(0x0083, id, 0x8006, &[]))
                        }
                    }
                    0x0084 => {
                        assert_eq!(length, 16);
                        let (address, length) = (u32_at(packet, 8), u32_at(packet, 12));
                        assert!(length <= 2048 && length.is_multiple_of(4));
                        if address < KEY + 4 && address + length > KEY {
                            state.snapshot.key_reads += 1;
                            Some(reply(0x0085, id, 0x8006, &[]))
                        } else {
                            let mut data = address.to_be_bytes().to_vec();
                            for a in (address..address + length).step_by(4) {
                                data.extend_from_slice(&state.word(a).to_be_bytes());
                            }
                            Some(reply(0x0085, id, 0, &data))
                        }
                    }
                    0x0086 => {
                        let address = u32_at(packet, 8);
                        let bytes = &packet[12..];
                        assert!(bytes.len().is_multiple_of(4));
                        if state.owner == Some(peer) {
                            for (index, raw) in bytes.as_chunks::<4>().0.iter().enumerate() {
                                state.put(
                                    address + u32::try_from(index).unwrap() * 4,
                                    u32::from_be_bytes(*raw),
                                );
                            }
                            Some(reply(
                                0x0087,
                                id,
                                0,
                                &[0, 0, 0, u8::try_from(bytes.len()).unwrap()],
                            ))
                        } else {
                            Some(reply(0x0087, id, 0x8006, &[]))
                        }
                    }
                    _ => panic!("unexpected synthetic camera command {command:04x}"),
                };
                drop(state);
                if let Some(response) = response {
                    control.send_to(&response, peer).unwrap();
                }
                for (packet, destination) in outgoing {
                    stream.send_to(&packet, destination).unwrap();
                }
            }
        });
        Self {
            address,
            state,
            stop,
            thread: Some(thread),
        }
    }

    pub(crate) fn open(&self) -> Camera {
        let mut backend = connect(self.address, Duration::from_secs(2)).unwrap();
        backend.xml_cache = Some(XML.into());
        backend.packet_size = PacketSize::Verified(1500);
        let mut info = crate::transport::simulator::info();
        info.id = "gige:020000000001".into();
        info.vendor = "Capturefab".into();
        info.model = "Synthetic Action".into();
        info.transport = Transport::GigE;
        info.address = Some(Ipv4Addr::LOCALHOST.to_string());
        Camera::from_backend(info, Box::new(backend)).unwrap()
    }

    pub(crate) fn allow_frames(&self, value: bool) {
        self.state.lock().unwrap().emit_frames = value;
    }
    pub(crate) fn snapshot(&self) -> Snapshot {
        self.state.lock().unwrap().snapshot.clone()
    }
    pub(crate) fn advance_clock(&self, ticks: u64) {
        {
            let mut state = self.state.lock().unwrap();
            assert!(ticks >= state.snapshot.device_ticks);
            state.snapshot.device_ticks = ticks;
            state.snapshot.events.push("clock_advance");
        }
        // The receive loop has a bounded timeout and emits only after seeing
        // this explicit update. Wait for its state transition, not host time.
        let until = Instant::now() + Duration::from_secs(1);
        while self
            .snapshot()
            .pending_action
            .is_some_and(|target| target <= ticks)
        {
            assert!(Instant::now() < until, "synthetic action was not released");
            thread::sleep(Duration::from_millis(1));
        }
    }

    pub(crate) fn issue(
        &self,
        device_key: u32,
        group_key: u32,
        group_mask: u32,
        acknowledged: bool,
    ) -> Result<Value> {
        self.issue_at(device_key, group_key, group_mask, acknowledged, None)
    }

    pub(crate) fn issue_scheduled(
        &self,
        device_key: u32,
        group_key: u32,
        group_mask: u32,
        action_time_ticks: u64,
    ) -> Result<Value> {
        self.issue_at(
            device_key,
            group_key,
            group_mask,
            true,
            Some(action_time_ticks),
        )
    }

    fn issue_at(
        &self,
        device_key: u32,
        group_key: u32,
        group_mask: u32,
        acknowledged: bool,
        action_time_ticks: Option<u64>,
    ) -> Result<Value> {
        let selected = if_addrs::get_if_addrs()?
            .into_iter()
            .find_map(|interface| {
                let if_addrs::IfAddr::V4(ip) = interface.addr else {
                    return None;
                };
                (ip.ip == Ipv4Addr::LOCALHOST).then_some(Interface {
                    name: interface.name,
                    index: interface.index.unwrap_or(0),
                    address: ip.ip,
                    netmask: ip.netmask,
                })
            })
            .context("loopback interface for synthetic action")?;
        let socket = Socket::open(&selected)?;
        let mut channel = LoopbackChannel {
            socket,
            destination: self.address,
        };
        let request = action::Request {
            mode: if acknowledged {
                action::Mode::Acknowledged
            } else {
                action::Mode::SentOnly
            },
            interface: selected.address,
            device_key,
            group_key,
            group_mask,
            expected: if acknowledged {
                vec![Ipv4Addr::LOCALHOST]
            } else {
                Vec::new()
            },
            timeout: Duration::from_secs(2),
            action_time_ticks,
        };
        let observed = self.snapshot().actions.len();
        let value = if acknowledged {
            action::test_fixture_exchange(&request, &selected, &mut channel, 0x3579, &|| false)
        } else {
            action::test_exchange(&request, &selected, &mut channel, 0x3579, &|| false)
        }?;
        // A private peer barrier makes workflow assertions causal. It is not
        // acknowledgement collection or a production sent-only guarantee.
        let until = Instant::now() + Duration::from_secs(1);
        while self.snapshot().actions.len() == observed {
            anyhow::ensure!(
                Instant::now() < until,
                "synthetic peer did not observe the action"
            );
            thread::sleep(Duration::from_millis(1));
        }
        Ok(value)
    }
}
impl Default for Harness {
    fn default() -> Self {
        Self::new()
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
struct LoopbackChannel {
    socket: Socket,
    destination: SocketAddrV4,
}
impl Channel for LoopbackChannel {
    fn send(&mut self, packet: &[u8]) -> std::io::Result<usize> {
        self.socket.test_send_to(packet, self.destination)
    }
    fn receive(&mut self, buffer: &mut [u8]) -> std::io::Result<Option<Datagram>> {
        self.socket.receive(buffer)
    }
    fn test_source_port(&self) -> u16 {
        self.destination.port()
    }
}
