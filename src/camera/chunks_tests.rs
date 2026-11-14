//! Chunk interpretation through the camera's acquisition/settings lifecycle.
use super::{
    Camera, Change,
    tests::{Device, OPEN_ACQUISITION},
};
use crate::{
    transport::chunks::{ByteOrder, ChunkData},
    types::{Backend, Frame, RegisterIo},
};
use anyhow::{Result, ensure};
use serde_json::Value;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

const SCALE: u64 = 0x70;
const PRESENCE: u64 = 0x74;
const RELOAD: u64 = 0x78;
const PAYLOAD: usize = 28;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Event {
    Read(u64),
    Write(u64),
    Start,
    Frame,
}
type Log = Arc<Mutex<Vec<Event>>>;
struct ChunkDevice {
    device: Device,
    log: Log,
}
impl ChunkDevice {
    fn note(&self, event: Event) {
        self.log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(event);
    }
}
impl RegisterIo for ChunkDevice {
    fn read_memory(&mut self, address: u64, length: usize) -> Result<Vec<u8>> {
        self.note(Event::Read(address));
        self.device.read_memory(address, length)
    }
    fn write_memory(&mut self, address: u64, bytes: &[u8]) -> Result<()> {
        self.note(Event::Write(address));
        if address == RELOAD {
            self.device.write_memory(SCALE, &4_u32.to_le_bytes())?;
        }
        self.device.write_memory(address, bytes)
    }
}
impl Backend for ChunkDevice {
    fn xml(&mut self) -> Result<String> {
        self.device.xml()
    }
    fn start(&mut self, payload: usize) -> Result<()> {
        ensure!(
            payload == PAYLOAD,
            "chunk payload was not included in acquisition size"
        );
        self.note(Event::Start);
        // The shared device fixture supplies the normalized16-byte image;
        // this transport wrapper attaches the additional12 wire bytes.
        self.device.start(16)
    }
    fn next_frame(&mut self, timeout: Duration) -> Result<Frame> {
        self.note(Event::Frame);
        let mut frame = self.device.next_frame(timeout)?;
        let mut payload = frame.data.to_vec();
        payload.extend_from_slice(&5_u32.to_le_bytes());
        payload.extend_from_slice(&0x12_u32.to_be_bytes());
        payload.extend_from_slice(&4_u32.to_be_bytes());
        let mut chunks = ChunkData::parse(&payload, 16, 16, ByteOrder::Big, 7)?;
        chunks.payload = payload.into();
        frame.chunks = Some(chunks.into());
        Ok(frame)
    }
    fn stop(&mut self) -> Result<()> {
        self.device.stop()
    }
}

fn fixture() -> (Camera, Log) {
    let nodes = format!(
        r#"{OPEN_ACQUISITION}
<Category Name="Fixture"><pFeature>Measurement</pFeature><pFeature>Scale</pFeature><pFeature>ChunkModeActive</pFeature><pFeature>Reload</pFeature></Category>
<Boolean Name="ChunkModeActive"><Value>true</Value></Boolean>
<Float Name="Measurement"><pIsImplemented>Present</pIsImplemented><pValue>Scaled</pValue><Unit>mm</Unit></Float>
<SwissKnife Name="Scaled"><pVariable Name="RAW">Raw</pVariable><pVariable Name="FACTOR">Scale</pVariable><Formula>RAW * FACTOR</Formula></SwissKnife>
<IntReg Name="Raw"><pPort>Chunk</pPort><Address>0</Address><Length>4</Length></IntReg><Port Name="Chunk"><ChunkID>00000012</ChunkID></Port>
<Integer Name="Scale"><pValue>ScaleReg</pValue></Integer><IntReg Name="ScaleReg"><Address>0x70</Address><Length>4</Length><AccessMode>RW</AccessMode></IntReg>
<IntReg Name="Present"><Address>0x74</Address><Length>4</Length><AccessMode>RO</AccessMode></IntReg>
<Command Name="Reload"><pValue>ReloadReg</pValue><CommandValue>1</CommandValue></Command><IntReg Name="ReloadReg"><Address>0x78</Address><Length>4</Length><AccessMode>WO</AccessMode></IntReg>"#
    );
    let log = Log::default();
    let device = Device::new(&nodes)
        .with(0, u32::try_from(PAYLOAD).unwrap())
        .with(SCALE, 2)
        .with(PRESENCE, 1);
    let camera = Camera::from_backend(
        crate::transport::simulator::info(),
        Box::new(ChunkDevice {
            device,
            log: log.clone(),
        }),
    )
    .unwrap();
    log.lock().unwrap().clear();
    (camera, log)
}
fn events(log: &Log) -> Vec<Event> {
    log.lock().unwrap().clone()
}
fn reads(log: &Log, address: u64) -> usize {
    events(log)
        .iter()
        .filter(|event| **event == Event::Read(address))
        .count()
}
fn interpreted(frame: &Frame) -> &Value {
    frame.chunks.as_ref().unwrap().values.as_ref().unwrap()
}
fn measurement(frame: &Frame) -> f64 {
    let value = interpreted(frame);
    assert_eq!(value["units"]["Measurement"], "mm");
    assert_eq!(value["errors"], serde_json::json!({}));
    value["values"]["Measurement"].as_f64().unwrap()
}

#[test]
fn start_freezes_live_dependencies_before_acquisition_and_frames_reuse_them() {
    let (mut camera, log) = fixture();
    camera.start().unwrap();
    assert_eq!(reads(&log, SCALE), 1);
    assert_eq!(reads(&log, PRESENCE), 1);
    let before = events(&log);
    let prepared = before
        .iter()
        .position(|event| *event == Event::Read(SCALE))
        .unwrap();
    let started = before
        .iter()
        .position(|event| *event == Event::Start)
        .unwrap();
    assert!(prepared < started);
    let read_count = before
        .iter()
        .filter(|event| matches!(event, Event::Read(_)))
        .count();
    for _ in 0..3 {
        let frame = camera.next_frame(Duration::from_secs(1)).unwrap();
        assert_eq!(measurement(&frame), 10.0);
        assert!(frame.received_monotonic_ns.is_some());
        assert_eq!(frame.chunks.as_ref().unwrap().payload.len(), PAYLOAD);
    }
    assert_eq!(
        events(&log)
            .iter()
            .filter(|event| matches!(event, Event::Read(_)))
            .count(),
        read_count
    );
    camera.stop().unwrap();
}

#[test]
fn settings_commands_and_raw_writes_refresh_interpretation_once() {
    let (mut camera, log) = fixture();
    camera.start().unwrap();
    assert_eq!(
        measurement(&camera.next_frame(Duration::from_secs(1)).unwrap()),
        10.0
    );
    camera.set("Scale", "3").unwrap();
    assert!(camera.chunk_context.is_none());
    assert_eq!(
        measurement(&camera.next_frame(Duration::from_secs(1)).unwrap()),
        15.0
    );
    assert_eq!(reads(&log, SCALE), 2);
    camera.execute("Reload").unwrap();
    assert!(camera.chunk_context.is_none());
    assert_eq!(
        measurement(&camera.next_frame(Duration::from_secs(1)).unwrap()),
        20.0
    );
    assert_eq!(reads(&log, SCALE), 3);
    camera.write_memory(SCALE, &5_u32.to_le_bytes()).unwrap();
    assert!(camera.chunk_context.is_none());
    assert_eq!(
        measurement(&camera.next_frame(Duration::from_secs(1)).unwrap()),
        25.0
    );
    assert_eq!(reads(&log, SCALE), 4);
    assert_eq!(
        measurement(&camera.next_frame(Duration::from_secs(1)).unwrap()),
        25.0
    );
    assert_eq!(reads(&log, SCALE), 4);
    camera.stop().unwrap();
}

#[test]
fn stale_frames_keep_wire_payload_and_wait_to_refresh_the_changed_context() {
    let (mut camera, log) = fixture();
    camera.start().unwrap();
    let first = camera.next_frame(Duration::from_secs(1)).unwrap();
    let original = first.chunks.as_ref().unwrap().payload.to_vec();
    camera.set("Scale", "9").unwrap();
    camera.settings_changed(Change::default());
    for _ in 0..2 {
        let frame = camera.next_frame(Duration::from_secs(1)).unwrap();
        assert!(camera.frame_predates_change());
        assert_eq!(interpreted(&frame)["values"], serde_json::json!({}));
        assert!(
            interpreted(&frame)["errors"]["context"]
                .as_str()
                .unwrap()
                .contains("predate")
        );
        assert_eq!(frame.chunks.as_ref().unwrap().payload.as_ref(), original);
        assert_eq!(reads(&log, SCALE), 1);
    }
    let frame = camera.next_frame(Duration::from_secs(1)).unwrap();
    assert!(!camera.frame_predates_change());
    assert_eq!(measurement(&frame), 45.0);
    assert_eq!(reads(&log, SCALE), 2);
    camera.stop().unwrap();
}
