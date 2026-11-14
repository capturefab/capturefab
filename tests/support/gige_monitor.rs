//! Independent synthetic external controller. It never takes instructions from a monitor.
use super::support::cli::{Home, Run};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io,
    net::{Ipv4Addr, SocketAddrV4, UdpSocket},
    os::unix::process::CommandExt as _,
    process::{Child, Stdio},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

pub const MAC: &str = "020000000001";
pub const PIXELS: [u8; 8] = [0x10, 0x21, 0x32, 0x43, 0x54, 0x65, 0x76, 0x87];
const GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 42, 19);
const XML_ADDRESS: usize = 0x1_0000;
const XML: &str = r#"<RegisterDescription VendorName="Independent" ModelName="ExternalController">
<Category Name="Root"><pFeature>Width</pFeature><pFeature>Height</pFeature><pFeature>PixelFormat</pFeature><pFeature>PayloadSize</pFeature><pFeature>TLParamsLocked</pFeature><pFeature>AcquisitionMode</pFeature><pFeature>TriggerSelector</pFeature><pFeature>TriggerMode</pFeature><pFeature>TriggerSource</pFeature><pFeature>TriggerSoftware</pFeature><pFeature>AcquisitionStart</pFeature><pFeature>AcquisitionStop</pFeature><pFeature>TransferControlMode</pFeature><pFeature>TransferStart</pFeature><pFeature>TransferStop</pFeature><pFeature>Buffer</pFeature><pFeature>ControllerPrivilege</pFeature></Category>
<Integer Name="Width"><pValue>WidthReg</pValue><Min>1</Min><Max>100</Max></Integer><IntReg Name="WidthReg"><Address>0x11000</Address><Length>4</Length><AccessMode>RW</AccessMode><Endianess>BigEndian</Endianess></IntReg>
<Integer Name="Height"><Value>2</Value></Integer><Integer Name="PayloadSize"><Value>8</Value></Integer>
<Enumeration Name="PixelFormat"><EnumEntry Name="Mono8"><Value>0x01080001</Value></EnumEntry><Value>0x01080001</Value></Enumeration>
<Integer Name="TLParamsLocked"><pValue>LockReg</pValue></Integer><IntReg Name="LockReg"><Address>0x11004</Address><Length>4</Length><AccessMode>RW</AccessMode><Endianess>BigEndian</Endianess></IntReg>
<Enumeration Name="AcquisitionMode"><pValue>ModeReg</pValue><EnumEntry Name="SingleFrame"><Value>0</Value></EnumEntry><EnumEntry Name="Continuous"><Value>2</Value></EnumEntry></Enumeration><IntReg Name="ModeReg"><Address>0x11008</Address><Length>4</Length><AccessMode>RW</AccessMode><Endianess>BigEndian</Endianess></IntReg>
<Enumeration Name="TriggerSelector"><pValue>SelectorReg</pValue><EnumEntry Name="FrameStart"><Value>0</Value></EnumEntry><EnumEntry Name="AcquisitionStart"><Value>1</Value></EnumEntry><pSelected>TriggerMode</pSelected><pSelected>TriggerSource</pSelected><pSelected>TriggerSoftware</pSelected></Enumeration><IntReg Name="SelectorReg"><Address>0x1100c</Address><Length>4</Length><AccessMode>RW</AccessMode><Endianess>BigEndian</Endianess></IntReg>
<Enumeration Name="TriggerMode"><EnumEntry Name="On"><Value>1</Value></EnumEntry><Value>1</Value></Enumeration><Enumeration Name="TriggerSource"><EnumEntry Name="Software"><Value>0</Value></EnumEntry><Value>0</Value></Enumeration>
<Command Name="TriggerSoftware"><pValue>CommandReg</pValue><CommandValue>1</CommandValue></Command><Command Name="AcquisitionStart"><pValue>CommandReg</pValue><CommandValue>2</CommandValue></Command><Command Name="AcquisitionStop"><pValue>CommandReg</pValue><CommandValue>3</CommandValue></Command><Command Name="TransferStart"><pValue>CommandReg</pValue><CommandValue>4</CommandValue></Command><Command Name="TransferStop"><pValue>CommandReg</pValue><CommandValue>5</CommandValue></Command><IntReg Name="CommandReg"><Address>0x11010</Address><Length>4</Length><AccessMode>WO</AccessMode><Endianess>BigEndian</Endianess></IntReg>
<Enumeration Name="TransferControlMode"><EnumEntry Name="UserControlled"><Value>2</Value></EnumEntry><Value>2</Value></Enumeration>
<Register Name="Buffer"><Address>0x11000</Address><Length>4</Length><AccessMode>RW</AccessMode></Register>
<IntReg Name="ControllerPrivilege"><Address>0xa00</Address><Length>4</Length><AccessMode>RO</AccessMode><Endianess>BigEndian</Endianess></IntReg>
<Enumeration Name="DeviceScanType"><EnumEntry Name="Areascan"><Value>0</Value></EnumEntry><Value>0</Value></Enumeration>
</RegisterDescription>"#;

pub fn hex(value: &str) -> Vec<u8> {
    let (pairs, tail) = value.as_bytes().as_chunks::<2>();
    assert_eq!(tail, b"");
    pairs
        .iter()
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}
pub fn literals() -> Vec<Vec<u8>> {
    let raw = include_bytes!("../fixtures/gige_monitor/literals.json");
    assert_eq!(
        format!("{:x}", Sha256::digest(raw)),
        "23418620be2de183695b3d95640b32224a98db17fc080db6b1471492a6e6d79c"
    );
    let value: Value = serde_json::from_slice(raw).unwrap();
    assert_eq!(hex(value["pixels_hex"].as_str().unwrap()), PIXELS);
    value["packets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|packet| {
            let raw = hex(packet["hex"].as_str().unwrap());
            assert_eq!(
                raw.len(),
                usize::try_from(packet["bytes"].as_u64().unwrap()).unwrap()
            );
            assert_eq!(
                format!("{:x}", Sha256::digest(&raw)),
                packet["sha256"].as_str().unwrap()
            );
            raw
        })
        .collect()
}

#[derive(Clone, Copy, Debug)]
pub enum Scenario {
    Frames,
    NoFrames,
    Malformed,
    Unicast,
    ZeroSource,
    NoCapability,
}
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub commands: Vec<u16>,
    pub reads: Vec<(u32, usize)>,
    pub ticks: u64,
    pub emitted: u64,
}
impl Snapshot {
    pub fn assert_passive(&self) {
        assert!(
            self.commands
                .iter()
                .all(|command| matches!(command, 0x0080 | 0x0084)),
            "unexpected control traffic: {:x?}",
            self.commands
        );
    }
}
type Shared = Arc<(Mutex<Snapshot>, Condvar)>;
pub struct Producer {
    shared: Shared,
    stopped: Arc<AtomicBool>,
    owner: Option<JoinHandle<()>>,
    pub destination_port: u16,
    pub source_port: u16,
}
fn word(memory: &mut [u8], address: usize, value: u32) {
    memory[address..address + 4].copy_from_slice(&value.to_be_bytes());
}
fn text(memory: &mut [u8], address: usize, value: &[u8]) {
    memory[address..address + value.len()].copy_from_slice(value);
}
fn reply(command: u16, id: u16, status: u16, body: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(8 + body.len());
    bytes.extend_from_slice(&status.to_be_bytes());
    bytes.extend_from_slice(&command.to_be_bytes());
    bytes.extend_from_slice(&u16::try_from(body.len()).unwrap().to_be_bytes());
    bytes.extend_from_slice(&id.to_be_bytes());
    bytes.extend_from_slice(body);
    bytes
}
fn read_u32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes(bytes.try_into().unwrap())
}
impl Producer {
    pub fn new(scenario: Scenario) -> Self {
        let control = UdpSocket::bind((Ipv4Addr::LOCALHOST, 3956)).unwrap();
        control
            .set_read_timeout(Some(Duration::from_millis(5)))
            .unwrap();
        control
            .set_write_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let reservation = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).unwrap();
        let destination_port = reservation.local_addr().unwrap().port();
        let stream = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let source_port = stream.local_addr().unwrap().port();
        socket2::SockRef::from(&stream)
            .set_multicast_if_v4(&Ipv4Addr::LOCALHOST)
            .unwrap();
        stream.set_multicast_loop_v4(true).unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let mut memory = vec![0; 0x1_2000];
        word(&mut memory, 4, 0);
        word(&mut memory, 8, 0x0200);
        word(&mut memory, 12, 1);
        word(&mut memory, 0x24, u32::from(Ipv4Addr::LOCALHOST));
        text(&mut memory, 0x48, b"Independent");
        text(&mut memory, 0x68, b"ExternalController");
        text(&mut memory, 0xd8, b"MONITOR-001");
        word(&mut memory, 0xa00, 2); // Existing controller remains control owner forever.
        word(&mut memory, 0xd00, u32::from(destination_port));
        word(&mut memory, 0xd04, 1500);
        let destination = if matches!(scenario, Scenario::Unicast) {
            Ipv4Addr::LOCALHOST
        } else {
            GROUP
        };
        word(&mut memory, 0xd18, u32::from(destination));
        word(
            &mut memory,
            0xd1c,
            if matches!(scenario, Scenario::ZeroSource) {
                0
            } else {
                u32::from(source_port)
            },
        );
        word(
            &mut memory,
            0x92c,
            if matches!(scenario, Scenario::NoCapability) {
                0
            } else {
                0x8000_0000
            },
        );
        word(&mut memory, 0x93c, 0);
        word(&mut memory, 0x940, 1_000_000_000);
        let url = format!("Local:Independent.xml;10000;{:x}", XML.len());
        text(&mut memory, 0x200, url.as_bytes());
        text(&mut memory, XML_ADDRESS, XML.as_bytes());
        for (address, value) in [(0x11000, 4), (0x11004, 1), (0x11008, 0), (0x1100c, 1)] {
            word(&mut memory, address, value);
        }
        let packets = literals();
        let shared: Shared = Arc::default();
        let stopped = Arc::new(AtomicBool::new(false));
        let (state, stop) = (shared.clone(), stopped.clone());
        let owner = thread::spawn(move || {
            let mut buffer = [0; 4096];
            let mut block = 42_u16;
            let mut next = Instant::now();
            while !stop.load(Ordering::Acquire) {
                match control.recv_from(&mut buffer) {
                    Ok((length, source)) => {
                        let request = &buffer[..length];
                        assert!(request.len() >= 8);
                        let (header, payload) = request.split_first_chunk::<8>().unwrap();
                        assert_eq!(header[0], 0x42);
                        let command = u16::from_be_bytes(header[2..4].try_into().unwrap());
                        let extent =
                            usize::from(u16::from_be_bytes(header[4..6].try_into().unwrap()));
                        let id = u16::from_be_bytes(header[6..8].try_into().unwrap());
                        assert_eq!(extent + 8, request.len());
                        let mut snapshot = state.0.lock().unwrap();
                        snapshot.commands.push(command);
                        let response = match command {
                            0x0080 => {
                                let (addresses, tail) = payload.as_chunks::<4>();
                                assert_eq!(tail, b"");
                                let mut body = Vec::new();
                                for address in addresses {
                                    let address = u32::from_be_bytes(*address);
                                    snapshot.reads.push((address, 4));
                                    let offset = usize::try_from(address).unwrap();
                                    body.extend_from_slice(&memory[offset..offset + 4]);
                                }
                                reply(0x0081, id, 0, &body)
                            }
                            0x0084 => {
                                assert_eq!(payload.len(), 8);
                                let address = read_u32(&payload[..4]);
                                let count = usize::try_from(read_u32(&payload[4..8])).unwrap();
                                assert!(count > 0 && count <= 2048);
                                snapshot.reads.push((address, count));
                                let offset = usize::try_from(address).unwrap();
                                let mut body = address.to_be_bytes().to_vec();
                                body.extend_from_slice(&memory[offset..offset + count]);
                                reply(0x0085, id, 0, &body)
                            }
                            _ => reply(command.checked_add(1).unwrap(), id, 0x8006, &[]),
                        };
                        state.1.notify_all();
                        drop(snapshot);
                        control.send_to(&response, source).unwrap();
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::WouldBlock
                                | io::ErrorKind::TimedOut
                                | io::ErrorKind::Interrupted
                        ) => {}
                    Err(error) => panic!("independent GVCP peer: {error}"),
                }
                if Instant::now() >= next {
                    if !matches!(scenario, Scenario::NoFrames) {
                        let destination = SocketAddrV4::new(destination, destination_port);
                        if matches!(scenario, Scenario::Malformed) {
                            stream.send_to(&[0, 0, 0], destination).unwrap();
                        } else {
                            for original in &packets {
                                let mut packet = original.clone();
                                assert!(packet.len() >= 4);
                                packet[2..4].copy_from_slice(&block.to_be_bytes());
                                stream.send_to(&packet, destination).unwrap();
                            }
                        }
                        block = block.wrapping_add(1).max(1);
                        state.0.lock().unwrap().emitted += 1;
                    }
                    state.0.lock().unwrap().ticks += 1;
                    state.1.notify_all();
                    next = Instant::now() + Duration::from_millis(15);
                }
            }
            assert_eq!(read_u32(&memory[0xa00..0xa04]), 2);
        });
        // No source can accidentally claim the destination reservation's port.
        drop(reservation);
        Self {
            shared,
            stopped,
            owner: Some(owner),
            destination_port,
            source_port,
        }
    }
    pub fn selector() -> String {
        format!("gige-monitor://127.0.0.1?interface=127.0.0.1&mac={MAC}")
    }
    pub fn snapshot(&self) -> Snapshot {
        self.shared.0.lock().unwrap().clone()
    }
    pub fn wait_after(&self, previous: u64) {
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut state = self.shared.0.lock().unwrap();
        while state.ticks <= previous {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(
                !left.is_zero(),
                "external controller stopped after a monitor exit"
            );
            state = self.shared.1.wait_timeout(state, left).unwrap().0;
        }
    }
    pub fn assert_passive(&self) {
        self.snapshot().assert_passive();
        assert!(
            !self.owner.as_ref().unwrap().is_finished(),
            "external controller exited"
        );
    }
}
impl Drop for Producer {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        if let Some(owner) = self.owner.take() {
            let deadline = Instant::now() + Duration::from_secs(2);
            while !owner.is_finished() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(2));
            }
            assert!(owner.is_finished(), "independent producer failed to stop");
            owner.join().unwrap();
        }
    }
}

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let pid = i32::try_from(self.0.id()).unwrap();
        // SAFETY: this fixture created this exact private process group.
        unsafe { libc::kill(-pid, libc::SIGKILL) };
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn output_file(home: &Home, sequence: u64, suffix: &str) -> File {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(home.path(&format!("command-{sequence}.{suffix}")))
        .unwrap()
}
pub fn run(home: &Home, args: &[&str]) -> Run {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
    let mut command = home.command();
    command
        .env("CAPTUREFAB_CACHE_DIR", home.root.join("cache"))
        .process_group(0)
        .args(args)
        .stdout(Stdio::from(output_file(home, sequence, "stdout")))
        .stderr(Stdio::from(output_file(home, sequence, "stderr")));
    let mut process = Process(command.spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        if let Some(status) = process.0.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "CLI did not finish: {args:?}");
        thread::sleep(Duration::from_millis(5));
    };
    let read = |suffix| {
        let path = home.path(&format!("command-{sequence}.{suffix}"));
        assert!(fs::metadata(&path).unwrap().len() <= 2 * 1024 * 1024);
        let bytes = fs::read(&path).unwrap();
        fs::remove_file(path).unwrap();
        bytes
    };
    let output = std::process::Output {
        status,
        stdout: read("stdout"),
        stderr: read("stderr"),
    };
    Run::of_output(output, args)
}

pub struct Sessions<'a> {
    home: &'a Home,
    names: Vec<&'static str>,
}
impl<'a> Sessions<'a> {
    pub fn new(home: &'a Home) -> Self {
        Self {
            home,
            names: vec![],
        }
    }
    pub fn start(&mut self, name: &'static str, camera: &str) {
        // Track before the command, so a partially admitted start is also stopped.
        self.names.push(name);
        run(
            self.home,
            &[
                "--json", "session", "start", "--name", name, "-c", camera, "--live",
            ],
        )
        .success();
    }
    pub fn stop(&mut self, name: &'static str) {
        run(self.home, &["--json", "session", "stop", name]).success();
        self.names.retain(|item| *item != name);
    }
}
impl Drop for Sessions<'_> {
    fn drop(&mut self) {
        for name in self.names.drain(..) {
            let _ = run(self.home, &["--json", "session", "stop", name]);
        }
    }
}
