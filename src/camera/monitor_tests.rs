//! Count every backend operation, including reads, across monitor lifecycles.
use super::{Camera, Change, monitor_identity};
use crate::{
    controls::{BankEntry, Name},
    transport::chunks::{ByteOrder, ChunkData},
    types::{Backend, CameraAccess, ErrorKind, Frame, RegisterIo, classify, control_remaining},
};
use anyhow::{Result, ensure};
use serde_json::json;
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const SELECTOR: &str = "gige-monitor://192.0.2.10?interface=192.0.2.1&mac=001122334455";
const XML: &str = r#"<RegisterDescription>
<Category Name="Root"><pFeature>PayloadSize</pFeature><pFeature>Gain</pFeature><pFeature>Buffer</pFeature><pFeature>Bank</pFeature><pFeature>Reload</pFeature><pFeature>Measurement</pFeature><pFeature>Raw</pFeature></Category>
<Integer Name="PayloadSize"><Value>16</Value></Integer>
<Integer Name="TLParamsLocked"><pValue>LockReg</pValue></Integer><IntReg Name="LockReg"><Address>0</Address><Length>4</Length><AccessMode>RW</AccessMode><Endianess>LittleEndian</Endianess></IntReg>
<Integer Name="Gain"><pValue>GainReg</pValue><Min>0</Min><Max>100</Max></Integer><IntReg Name="GainReg"><Address>4</Address><Length>4</Length><AccessMode>RW</AccessMode><Endianess>LittleEndian</Endianess></IntReg>
<Enumeration Name="Bank"><pValue>BankReg</pValue><pSelected>Buffer</pSelected><EnumEntry Name="A"><Value>0</Value></EnumEntry><EnumEntry Name="B"><Value>1</Value></EnumEntry></Enumeration><IntReg Name="BankReg"><Address>8</Address><Length>4</Length><AccessMode>RW</AccessMode><Endianess>LittleEndian</Endianess></IntReg>
<Register Name="Buffer"><Address>4</Address><Length>4</Length><AccessMode>RW</AccessMode></Register>
<Integer Name="Scale"><pValue>ScaleReg</pValue></Integer><IntReg Name="ScaleReg"><Address>12</Address><Length>4</Length><AccessMode>RO</AccessMode><Endianess>LittleEndian</Endianess></IntReg>
<Command Name="AcquisitionStart"><pValue>StartReg</pValue><CommandValue>1</CommandValue></Command><IntReg Name="StartReg"><Address>16</Address><Length>4</Length><AccessMode>WO</AccessMode></IntReg>
<Command Name="AcquisitionStop"><pValue>StopReg</pValue><CommandValue>1</CommandValue></Command><IntReg Name="StopReg"><Address>20</Address><Length>4</Length><AccessMode>WO</AccessMode></IntReg>
<Enumeration Name="TransferControlMode"><Value>2</Value><EnumEntry Name="UserControlled"><Value>2</Value></EnumEntry></Enumeration>
<Command Name="TransferStart"><pValue>TransferStartReg</pValue><CommandValue>1</CommandValue></Command><IntReg Name="TransferStartReg"><Address>24</Address><Length>4</Length><AccessMode>WO</AccessMode></IntReg>
<Command Name="TransferStop"><pValue>TransferStopReg</pValue><CommandValue>1</CommandValue></Command><IntReg Name="TransferStopReg"><Address>28</Address><Length>4</Length><AccessMode>WO</AccessMode></IntReg>
<Command Name="Reload"><pValue>StartReg</pValue><CommandValue>1</CommandValue></Command>
<Enumeration Name="AcquisitionMode"><Value>0</Value><EnumEntry Name="SingleFrame"><Value>0</Value></EnumEntry><EnumEntry Name="Continuous"><Value>2</Value></EnumEntry></Enumeration>
<Enumeration Name="TriggerSelector"><pValue>BankReg</pValue><EnumEntry Name="FrameStart"><Value>1</Value></EnumEntry><pSelected>TriggerMode</pSelected><pSelected>TriggerSource</pSelected></Enumeration>
<Enumeration Name="TriggerMode"><Value>1</Value><EnumEntry Name="On"><Value>1</Value></EnumEntry></Enumeration>
<Enumeration Name="TriggerSource"><Value>0</Value><EnumEntry Name="Software"><Value>0</Value></EnumEntry></Enumeration>
<Command Name="TriggerSoftware"><pValue>StartReg</pValue><CommandValue>1</CommandValue></Command>
<IntReg Name="ActionDeviceKey"><Address>36</Address><Length>4</Length><AccessMode>WO</AccessMode></IntReg>
<Boolean Name="ChunkModeActive"><Value>true</Value></Boolean>
<Integer Name="Raw"><pValue>ChunkRaw</pValue></Integer><IntReg Name="ChunkRaw"><pPort>Chunk</pPort><Address>0</Address><Length>4</Length><Endianess>LittleEndian</Endianess></IntReg><Port Name="Chunk"><ChunkID>00000012</ChunkID></Port>
<IntSwissKnife Name="Measurement"><pVariable Name="RAW">Raw</pVariable><pVariable Name="SCALE">Scale</pVariable><Formula>RAW * SCALE</Formula></IntSwissKnife>
<Enumeration Name="DeviceScanType"><pValue>ScanReg</pValue><EnumEntry Name="Areascan"><Value>0</Value></EnumEntry></Enumeration><IntReg Name="ScanReg"><Address>40</Address><Length>4</Length><AccessMode>RO</AccessMode></IntReg>
</RegisterDescription>"#;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Event {
    Access,
    Xml,
    Read(u64, usize),
    Write(u64, usize),
    Initialized,
    Payload,
    Start(usize),
    Request,
    Frame,
    Stop,
    FinishStop,
    Buffered,
    Clock,
    Checkpoint,
    Restore,
    Rest,
    Optimize,
    Drop,
}
type Log = Arc<Mutex<Vec<Event>>>;
struct Monitor {
    log: Log,
    access: CameraAccess,
    initial: Option<Instant>,
    invalid_xml: bool,
    expired_finish: bool,
    failed_start: bool,
    failed_frame: bool,
    failed_stop: bool,
    streaming: bool,
}
impl Monitor {
    fn new(log: &Log) -> Self {
        Self {
            log: log.clone(),
            access: CameraAccess::ReadOnlyMonitor,
            initial: Some(Instant::now() + Duration::from_secs(2)),
            invalid_xml: false,
            expired_finish: false,
            failed_start: false,
            failed_frame: false,
            failed_stop: false,
            streaming: false,
        }
    }
    fn note(&self, event: Event) {
        self.log.lock().unwrap().push(event);
    }
}
impl RegisterIo for Monitor {
    fn read_memory(&mut self, address: u64, length: usize) -> Result<Vec<u8>> {
        if let Some(deadline) = self.initial {
            control_remaining(deadline)?;
        }
        self.note(Event::Read(address, length));
        let word = match address {
            0 => 1_u32,
            4 => 7,
            12 => 2,
            _ => 0,
        };
        ensure!(length == 4, "unexpected monitor read length");
        Ok(word.to_le_bytes().to_vec())
    }
    fn read_memory_until(
        &mut self,
        address: u64,
        length: usize,
        deadline: Instant,
    ) -> Result<Vec<u8>> {
        control_remaining(deadline)?;
        self.read_memory(address, length)
    }
    fn write_memory(&mut self, address: u64, data: &[u8]) -> Result<()> {
        self.note(Event::Write(address, data.len()));
        anyhow::bail!("a monitor must never reach backend writes")
    }
    fn write_memory_until(&mut self, address: u64, data: &[u8], _: Instant) -> Result<()> {
        self.write_memory(address, data)
    }
}
impl Backend for Monitor {
    fn access(&self) -> CameraAccess {
        self.note(Event::Access);
        self.access
    }
    fn xml(&mut self) -> Result<String> {
        self.note(Event::Xml);
        control_remaining(self.initial.unwrap())?;
        Ok(if self.invalid_xml { "<broken" } else { XML }.into())
    }
    fn finish_initialization(&mut self) -> Result<()> {
        self.note(Event::Initialized);
        if self.expired_finish {
            return control_remaining(Instant::now().checked_sub(Duration::from_secs(1)).unwrap())
                .map(|_| ());
        }
        control_remaining(self.initial.unwrap())?;
        self.initial = None;
        // A later capability query must not change the Camera's decision.
        self.access = CameraAccess::Control;
        Ok(())
    }
    fn stream_payload_size(&mut self) -> Result<Option<usize>> {
        self.note(Event::Payload);
        Ok(Some(16))
    }
    fn start(&mut self, size: usize) -> Result<()> {
        self.note(Event::Start(size));
        ensure!(!self.failed_start, "host receive start failed");
        self.streaming = true;
        Ok(())
    }
    fn request_frame(&mut self) -> Result<()> {
        self.note(Event::Request);
        ensure!(self.streaming, "host receiver is stopped");
        Ok(())
    }
    fn next_frame(&mut self, _: Duration) -> Result<Frame> {
        self.note(Event::Frame);
        ensure!(!self.failed_frame, "host receive failed");
        let mut payload = vec![0x11; 16];
        payload.extend_from_slice(&5_u32.to_le_bytes());
        payload.extend_from_slice(&0x12_u32.to_be_bytes());
        payload.extend_from_slice(&4_u32.to_be_bytes());
        let mut chunks = ChunkData::parse(&payload, 16, 16, ByteOrder::Big, 7)?;
        chunks.payload = payload.into();
        Ok(Frame {
            id: 42,
            width: 4,
            height: 4,
            pixel_format: crate::types::MONO8,
            data: vec![0x11; 16].into(),
            chunks: Some(chunks.into()),
            ..Frame::default()
        })
    }
    fn stop_after_frame_error(&self) -> bool {
        true
    }
    fn stop(&mut self) -> Result<()> {
        self.note(Event::Stop);
        self.streaming = false;
        ensure!(!self.failed_stop, "host stop failed");
        Ok(())
    }
    fn finish_stop(&mut self) {
        self.note(Event::FinishStop);
    }
    fn buffered_frames(&mut self) -> u32 {
        self.note(Event::Buffered);
        99
    }
    fn device_time_ns(&mut self) -> Option<u64> {
        self.note(Event::Clock);
        Some(99)
    }
    fn emulated_settings(&mut self) -> Option<serde_json::Value> {
        self.note(Event::Checkpoint);
        Some(json!({"Gain":7}))
    }
    fn restore_settings(&mut self, _: &serde_json::Value) -> Result<()> {
        self.note(Event::Restore);
        Ok(())
    }
    fn rest(&mut self) {
        self.note(Event::Rest);
    }
    fn optimize_network(&mut self, _: usize) -> Result<crate::network::NetworkTuning> {
        self.note(Event::Optimize);
        anyhow::bail!("unexpected monitor optimization")
    }
}
impl Drop for Monitor {
    fn drop(&mut self) {
        self.note(Event::Drop);
    }
}
fn info() -> crate::types::CameraInfo {
    let mut info = crate::transport::simulator::info();
    info.id = SELECTOR.into();
    info.transport = crate::types::Transport::GigE;
    info.address = Some("192.0.2.10".into());
    info
}
fn opened() -> (Camera, Log) {
    let log = Log::default();
    let camera = Camera::from_backend(info(), Box::new(Monitor::new(&log))).unwrap();
    (camera, log)
}
fn take(log: &Log) -> Vec<Event> {
    std::mem::take(&mut *log.lock().unwrap())
}
fn failed<T>(result: Result<T>) -> anyhow::Error {
    match result {
        Ok(_) => panic!("operation unexpectedly succeeded"),
        Err(error) => error,
    }
}
fn unsupported<T>(result: Result<T>) {
    assert_eq!(classify(&failed(result)).kind, ErrorKind::Unsupported);
}

#[test]
fn constructor_captures_access_once_and_never_unlocks_device() {
    let (mut camera, log) = opened();
    let events = take(&log);
    assert_eq!(events.first(), Some(&Event::Access));
    assert_eq!(events.last(), Some(&Event::Initialized));
    assert_eq!(
        events
            .iter()
            .filter(|event| **event == Event::Access)
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Event::Write(..)))
            .count(),
        0
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Event::Read(0, _)))
            .count(),
        0
    );
    assert!(events.contains(&Event::Read(40, 4)));
    assert_eq!(camera.access(), CameraAccess::ReadOnlyMonitor);
    assert!(camera.is_read_only_monitor());
    assert_eq!(camera.get("Gain").unwrap(), 7);
    assert!(!camera.is_writable("Gain"));
    assert!(!camera.is_configuration_value("Gain"));
    assert!(!camera.feature("Gain").unwrap().writable);
    for feature in camera.features() {
        assert!(!feature.writable, "{}", feature.name);
    }
    let bytes = camera
        .read_register_feature(
            &Name {
                name: "Buffer".into(),
                bank: vec![],
            },
            Duration::from_secs(1),
        )
        .unwrap();
    assert_eq!(bytes["hex"], "07000000");
    assert_eq!(camera.read_memory(4, 4).unwrap(), 7_u32.to_le_bytes());
    drop(camera);
    let events = take(&log);
    assert_eq!(events.last(), Some(&Event::Drop));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Event::Write(..) | Event::Access))
            .count(),
        0
    );
}

#[test]
fn forbidden_operations_fail_before_even_dependency_reads() {
    let (mut camera, log) = opened();
    take(&log);
    unsupported(camera.set("Gain", "8"));
    unsupported(camera.validate_set("Gain", "8"));
    unsupported(camera.execute("Reload"));
    unsupported(camera.write_memory(4, &[0; 4]));
    unsupported(camera.write_action_device_key(1, Instant::now() + Duration::from_secs(1)));
    let name = Name {
        name: "Buffer".into(),
        bank: vec![BankEntry {
            selector: Some("Bank".into()),
            value: "B".into(),
        }],
    };
    unsupported(camera.read_register_feature(&name, Duration::from_secs(1)));
    unsupported(camera.write_register_feature(&name, &[0; 4], Duration::from_secs(1)));
    // The child transfer API also guards before resolving selectors or layout.
    unsupported(super::register::transfer(
        &mut camera,
        &name,
        None,
        Duration::from_secs(1),
    ));
    unsupported(camera.restore_settings(&json!({"Gain":8})));
    unsupported(camera.optimize_network());
    assert_eq!(camera.emulated_settings(), None);
    assert!(!camera.software_triggered());
    camera.rest();
    assert_eq!(take(&log), []);
}

#[test]
fn capture_drop_and_chunk_interpretation_use_host_receiver_only() {
    let (mut camera, log) = opened();
    take(&log);
    camera.start().unwrap();
    camera.start().unwrap();
    assert!(!camera.software_triggered());
    camera.request_frame().unwrap();
    camera.settings_changed(Change::default());
    let frame = camera.next_frame(Duration::from_secs(1)).unwrap();
    assert_eq!(frame.id, 42);
    assert_eq!(&*frame.data, &[0x11; 16]);
    let chunks = frame.chunks.unwrap();
    assert_eq!(&chunks.payload[16..20], &5_u32.to_le_bytes());
    assert_eq!(chunks.payload_bytes, 28);
    assert_eq!(chunks.values.as_ref().unwrap()["values"]["Raw"], 5);
    assert!(
        chunks.values.as_ref().unwrap()["errors"]["Measurement"]
            .as_str()
            .unwrap()
            .contains("live dependency")
    );
    assert!(!camera.frame_predates_change());
    drop(camera);
    assert_eq!(
        take(&log),
        [
            Event::Payload,
            Event::Start(16),
            Event::Request,
            Event::Frame,
            Event::Stop,
            Event::FinishStop,
            Event::Drop
        ]
    );
}

#[test]
fn initialization_and_capture_failures_drop_without_device_cleanup_writes() {
    for invalid_xml in [false, true] {
        let log = Log::default();
        let mut backend = Monitor::new(&log);
        backend.invalid_xml = invalid_xml;
        backend.expired_finish = !invalid_xml;
        let error = failed(Camera::from_backend(info(), Box::new(backend)));
        if !invalid_xml {
            assert_eq!(classify(&error).kind, ErrorKind::Timeout);
        }
        let events = take(&log);
        assert_eq!(events.last(), Some(&Event::Drop));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, Event::Write(..)))
                .count(),
            0
        );
    }
    for failed_start in [false, true] {
        let log = Log::default();
        let mut backend = Monitor::new(&log);
        backend.failed_start = failed_start;
        backend.failed_frame = !failed_start;
        backend.failed_stop = !failed_start;
        let mut camera = Camera::from_backend(info(), Box::new(backend)).unwrap();
        take(&log);
        if failed_start {
            failed(camera.start());
        } else {
            camera.start().unwrap();
            failed(camera.next_frame(Duration::from_secs(1)));
            assert!(!camera.is_streaming());
        }
        drop(camera);
        let events = take(&log);
        assert_eq!(events.last(), Some(&Event::Drop));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, Event::Write(..) | Event::Read(..)))
                .count(),
            0
        );
    }
}

#[test]
fn identified_monitor_requires_original_selector_and_exact_canonical_identity() {
    let spec = crate::transport::gige::monitor::MonitorSpec::parse(SELECTOR).unwrap();
    monitor_identity(&spec, &info()).unwrap();
    for field in ["id", "address", "transport"] {
        let mut changed = info();
        match field {
            "id" => changed.id = "gige-monitor:".into(),
            "address" => changed.address = Some("192.0.2.11".into()),
            _ => changed.transport = crate::types::Transport::Simulator,
        }
        assert_eq!(
            classify(&failed(Camera::open_identified(
                SELECTOR,
                changed,
                Duration::ZERO,
                None
            )))
            .kind,
            ErrorKind::Usage
        );
    }
    assert_eq!(
        classify(&failed(Camera::open_identified(
            "gige:192.0.2.10",
            info(),
            Duration::ZERO,
            None
        )))
        .kind,
        ErrorKind::Usage
    );
    assert_eq!(
        classify(&failed(Camera::open_identified(
            "gige-monitor:",
            info(),
            Duration::ZERO,
            None
        )))
        .kind,
        ErrorKind::Usage
    );
}
