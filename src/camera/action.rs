//! Explicit key writes use the current owner and one caller-supplied deadline.
use super::Camera;
use crate::types::{Backend, Categorized, ErrorKind, RegisterIo, Transport, control_remaining};
use anyhow::Result;
use serde_json::{Value, json};
use std::{sync::Arc, time::Instant};

const FEATURE: &str = "ActionDeviceKey";

#[derive(Debug)]
struct SharedFailure(Arc<anyhow::Error>);
impl std::fmt::Display for SharedFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ActionDeviceKey dependency or write failed")
    }
}
impl std::error::Error for SharedFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref().as_ref())
    }
}

struct DeadlineIo<'a> {
    backend: &'a mut dyn Backend,
    deadline: Instant,
    address: u64,
    length: usize,
    native_gige: bool,
    attempted: &'a mut bool,
    failure: Option<Arc<anyhow::Error>>,
}
impl DeadlineIo<'_> {
    fn check(&self) -> Result<()> {
        if let Some(error) = &self.failure {
            return Err(SharedFailure(error.clone()).into());
        }
        if let Some(error) = self.backend.terminal_error() {
            return Err(error);
        }
        control_remaining(self.deadline).map(|_| ())
    }
    fn track<T>(&mut self, result: Result<T>) -> Result<T> {
        result.map_err(|error| {
            let error = Arc::new(error);
            self.failure.get_or_insert_with(|| error.clone());
            SharedFailure(error).into()
        })
    }
    fn disjoint_read(&self, address: u64, length: usize) -> Result<()> {
        let mut end = address.checked_add(u64::try_from(length)?).ok_or_else(|| {
            Categorized::error(
                ErrorKind::Unsupported,
                "ActionDeviceKey dependency range overflow",
            )
        })?;
        let start = if self.native_gige {
            end = end.checked_add(3).ok_or_else(|| {
                Categorized::error(
                    ErrorKind::Unsupported,
                    "ActionDeviceKey aligned dependency range overflow",
                )
            })? & !3;
            address & !3
        } else {
            address
        };
        let key_end = self
            .address
            .checked_add(u64::try_from(self.length)?)
            .ok_or_else(|| {
                Categorized::error(ErrorKind::Unsupported, "ActionDeviceKey range overflow")
            })?;
        if length == 0 || (start < key_end && self.address < end) {
            return Err(Categorized::error(
                ErrorKind::Unsupported,
                "ActionDeviceKey dependencies cannot read key bytes or an overlapping native read span",
            ));
        }
        Ok(())
    }
}
impl RegisterIo for DeadlineIo<'_> {
    fn read_memory(&mut self, address: u64, length: usize) -> Result<Vec<u8>> {
        let result = (|| {
            self.check()?;
            self.disjoint_read(address, length)?;
            let bytes = self
                .backend
                .read_memory_until(address, length, self.deadline)?;
            self.check()?;
            if bytes.len() != length {
                return Err(Categorized::error(
                    ErrorKind::Incomplete,
                    "short ActionDeviceKey dependency read",
                ));
            }
            Ok(bytes)
        })();
        self.track(result)
    }
    fn write_memory(&mut self, address: u64, data: &[u8]) -> Result<()> {
        let result = (|| {
            self.check()?;
            if address != self.address || data.len() != self.length {
                return Err(Categorized::error(
                    ErrorKind::Unsupported,
                    "ActionDeviceKey must write one complete preflighted register",
                ));
            }
            *self.attempted = true;
            self.backend
                .write_memory_until(address, data, self.deadline)?;
            self.check()
        })();
        self.track(result)
    }
}

pub(super) fn write(camera: &mut Camera, key: u32, deadline: Instant) -> Result<Value> {
    camera.require_control()?;
    let mut attempted = false;
    let result = transfer(camera, key, deadline, &mut attempted);
    result.map_err(|error| {
        crate::types::with_details(
            error,
            json!({
                "camera":camera.info.id,"feature":FEATURE,"write_attempted":attempted,
                "write_may_have_applied":attempted,"readback":"unavailable_write_only",
                "rollback":"not_available","scope":"current_control_epoch"
            }),
        )
    })
}

fn transfer(
    camera: &mut Camera,
    key: u32,
    deadline: Instant,
    attempted: &mut bool,
) -> Result<Value> {
    if let Some(error) = camera.backend.terminal_error() {
        return Err(error);
    }
    control_remaining(deadline)?;
    let completion = match camera.info.transport {
        Transport::GigE | Transport::Usb3 => "native_transport_ack",
        Transport::GenTl => "producer_reported",
        Transport::Simulator | Transport::Media => {
            return Err(Categorized::error(
                ErrorKind::Unsupported,
                "this source does not expose a native camera key owner",
            ));
        }
    };
    // Structural/static resolution cannot read the key via layout dependencies.
    let target = camera.nodes.action_device_key_target()?;
    let native_gige = camera.info.transport == Transport::GigE;
    if native_gige && (!target.address().is_multiple_of(4) || !target.length().is_multiple_of(4)) {
        return Err(Categorized::error(
            ErrorKind::Unsupported,
            "native GigE ActionDeviceKey writes require a four-byte aligned complete register; implicit read-modify-write is unavailable",
        ));
    }
    if native_gige {
        let address = u32::try_from(target.address()).map_err(|_| {
            Categorized::error(
                ErrorKind::Unsupported,
                "native GigE ActionDeviceKey address exceeds 32 bits",
            )
        })?;
        if target
            .address()
            .checked_add(u64::try_from(target.length())?)
            .is_none_or(|end| end > u64::from(u32::MAX) + 1)
        {
            return Err(Categorized::error(
                ErrorKind::Unsupported,
                "native GigE ActionDeviceKey span exceeds the 32-bit address space",
            ));
        }
        if crate::transport::gige::managed_range(address, target.length()) {
            return Err(Categorized::error(
                ErrorKind::Unsupported,
                "ActionDeviceKey cannot target a transport-managed controller register",
            ));
        }
    }
    let mut io = DeadlineIo {
        backend: camera.backend.as_mut(),
        deadline,
        address: target.address(),
        length: target.length(),
        native_gige,
        attempted,
        failure: None,
    };
    io.check()?;
    let result = camera.nodes.write_action_device_key(&mut io, &target, key);
    // Never replace the first typed I/O/ownership cause with metadata or timeout.
    if let Some(error) = &io.failure {
        return Err(SharedFailure(error.clone()).into());
    }
    result?;
    io.check()?;
    let result = json!({"camera":camera.info.id,"feature":FEATURE,"write_completed":true,
        "completion":completion,"readback":"unavailable_write_only",
        "rollback":"not_available","scope":"current_control_epoch"});
    io.check()?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Frame, classify};
    use anyhow::Context as _;
    use std::{collections::BTreeMap, sync::Mutex, time::Duration};

    #[derive(Clone, Copy, Default, PartialEq, Eq)]
    enum Mode {
        #[default]
        Normal,
        ShortRead,
        LateRead,
        RetiredRead,
        LateWrite,
        PartialWrite,
        RetiredWrite,
    }
    #[derive(Default)]
    struct State {
        bytes: BTreeMap<u64, u8>,
        reads: Vec<(u64, usize)>,
        writes: Vec<(u64, Vec<u8>)>,
        deadlines: Vec<Instant>,
        unbounded: usize,
        mode: Mode,
        retired: bool,
    }
    struct Device {
        xml: String,
        state: Arc<Mutex<State>>,
    }
    fn retired() -> anyhow::Error {
        Categorized::new(ErrorKind::Disconnected, "fixture owner retired")
            .hint("explicit Connect required")
            .details(json!({"controller_ownership":"retired","probe_code":7}))
            .into()
    }
    impl RegisterIo for Device {
        fn read_memory(&mut self, _: u64, _: usize) -> Result<Vec<u8>> {
            self.state
                .lock()
                .map_err(|_| anyhow::anyhow!("fixture mutex poisoned"))?
                .unbounded += 1;
            Err(anyhow::anyhow!("unbounded fixture read"))
        }
        fn write_memory(&mut self, _: u64, _: &[u8]) -> Result<()> {
            self.state
                .lock()
                .map_err(|_| anyhow::anyhow!("fixture mutex poisoned"))?
                .unbounded += 1;
            Err(anyhow::anyhow!("unbounded fixture write"))
        }
        fn read_memory_until(
            &mut self,
            address: u64,
            length: usize,
            deadline: Instant,
        ) -> Result<Vec<u8>> {
            control_remaining(deadline)?;
            let mut state = self
                .state
                .lock()
                .map_err(|_| anyhow::anyhow!("fixture mutex poisoned"))?;
            state.reads.push((address, length));
            state.deadlines.push(deadline);
            let mode = state.mode;
            if mode == Mode::RetiredRead {
                state.retired = true;
                return Err(retired());
            }
            let mut bytes = Vec::with_capacity(length);
            for index in 0..length {
                let address = address
                    .checked_add(u64::try_from(index)?)
                    .context("fixture range overflow")?;
                bytes.push(state.bytes.get(&address).copied().unwrap_or(0));
            }
            if mode == Mode::ShortRead {
                bytes.pop();
            }
            drop(state);
            if mode == Mode::LateRead {
                std::thread::sleep(
                    deadline.saturating_duration_since(Instant::now()) + Duration::from_millis(2),
                );
            }
            Ok(bytes)
        }
        fn write_memory_until(
            &mut self,
            address: u64,
            data: &[u8],
            deadline: Instant,
        ) -> Result<()> {
            control_remaining(deadline)?;
            let mut state = self
                .state
                .lock()
                .map_err(|_| anyhow::anyhow!("fixture mutex poisoned"))?;
            state.writes.push((address, data.to_vec()));
            state.deadlines.push(deadline);
            let mode = state.mode;
            let count = if matches!(mode, Mode::PartialWrite | Mode::RetiredWrite) {
                1
            } else {
                data.len()
            };
            for (index, byte) in data.iter().take(count).enumerate() {
                let address = address
                    .checked_add(u64::try_from(index)?)
                    .context("fixture range overflow")?;
                state.bytes.insert(address, *byte);
            }
            if mode == Mode::RetiredWrite {
                state.retired = true;
                return Err(retired());
            }
            if mode == Mode::PartialWrite {
                return Err(Categorized::new(
                    ErrorKind::Incomplete,
                    "short fixture native completion",
                )
                .details(json!({"written_prefix_bytes":1}))
                .into());
            }
            drop(state);
            if mode == Mode::LateWrite {
                std::thread::sleep(
                    deadline.saturating_duration_since(Instant::now()) + Duration::from_millis(2),
                );
            }
            Ok(())
        }
    }
    impl Backend for Device {
        fn xml(&mut self) -> Result<String> {
            Ok(self.xml.clone())
        }
        fn start(&mut self, _: usize) -> Result<()> {
            Ok(())
        }
        fn next_frame(&mut self, _: Duration) -> Result<Frame> {
            Err(anyhow::anyhow!("no fixture frames"))
        }
        fn stop(&mut self) -> Result<()> {
            Ok(())
        }
        fn terminal_error(&self) -> Option<anyhow::Error> {
            match self.state.lock() {
                Ok(state) if state.retired => Some(retired()),
                Ok(_) => None,
                Err(_) => Some(anyhow::anyhow!("fixture mutex poisoned")),
            }
        }
    }
    fn fixture(body: &str, transport: Transport) -> (Camera, Arc<Mutex<State>>) {
        let state = Arc::new(Mutex::new(State::default()));
        let mut info = crate::transport::simulator::info();
        info.id = "fake-action-key".into();
        info.transport = transport;
        let camera = Camera::from_backend(
            info,
            Box::new(Device {
                xml: format!("<RegisterDescription>{body}</RegisterDescription>"),
                state: state.clone(),
            }),
        )
        .unwrap();
        {
            let mut state = state.lock().unwrap();
            state.reads.clear();
            state.writes.clear();
            state.deadlines.clear();
            state.unbounded = 0;
        }
        (camera, state)
    }
    fn key(extra: &str, layout: &str) -> String {
        format!(
            "<Integer Name='ActionDeviceKey'><AccessMode>WO</AccessMode>{extra}<pValue>Key</pValue></Integer><IntReg Name='Key'>{layout}</IntReg>"
        )
    }
    const LAYOUT: &str =
        "<Address>0x11000</Address><Length>4</Length><Endianess>LittleEndian</Endianess>";
    fn word(state: &mut State, address: u64, value: u32) {
        for (index, byte) in value.to_le_bytes().into_iter().enumerate() {
            state
                .bytes
                .insert(address + u64::try_from(index).unwrap(), byte);
        }
    }
    fn flags(error: &anyhow::Error, attempted: bool) {
        let details = classify(error).details.unwrap();
        assert_eq!(details["write_attempted"], attempted);
        assert_eq!(details["write_may_have_applied"], attempted);
        assert_eq!(details["feature"], FEATURE);
        assert!(details.get("key").is_none());
    }

    #[test]
    fn exact_owned_native_or_producer_completion_has_no_key_or_readback_fact() {
        for (transport, completion) in [
            (Transport::GigE, "native_transport_ack"),
            (Transport::Usb3, "native_transport_ack"),
            (Transport::GenTl, "producer_reported"),
        ] {
            for (endian, bytes) in [
                ("LittleEndian", [0x78, 0x56, 0x34, 0x12]),
                ("BigEndian", [0x12, 0x34, 0x56, 0x78]),
            ] {
                let layout = format!(
                    "<Address>0x11000</Address><Length>4</Length><Endianess>{endian}</Endianess>"
                );
                let (mut camera, state) = fixture(&key("", &layout), transport);
                camera.software_triggers = Some(vec![None]);
                let deadline = Instant::now() + Duration::from_secs(1);
                let result = camera
                    .write_action_device_key(0x1234_5678, deadline)
                    .unwrap();
                assert_eq!(
                    result,
                    json!({"camera":"fake-action-key","feature":"ActionDeviceKey",
                    "write_completed":true,"completion":completion,"readback":"unavailable_write_only",
                    "rollback":"not_available","scope":"current_control_epoch"})
                );
                assert_eq!(camera.software_triggers, Some(vec![None]));
                let state = state.lock().unwrap();
                assert_eq!(state.writes, [(0x0001_1000, bytes.to_vec())]);
                assert_eq!(state.reads, [] as [(u64, usize); 0]);
                assert_eq!(state.deadlines, [deadline]);
                assert_eq!(state.unbounded, 0);
            }
        }
    }

    #[test]
    fn bad_access_shapes_and_native_managed_or_rmw_targets_fail_before_io() {
        let mut bodies = vec![
            "<Integer Name='ActionDeviceKey'><Value>1</Value><AccessMode>WO</AccessMode></Integer>"
                .to_owned(),
            key("<pValueCopy>Key</pValueCopy>", LAYOUT),
            key("", &format!("{LAYOUT}<AccessMode>RO</AccessMode>")),
            key(
                "",
                "<Address>0x11001</Address><Length>4</Length><Endianess>LittleEndian</Endianess>",
            ),
            key(
                "",
                "<Address>0x11000</Address><Length>2</Length><Endianess>LittleEndian</Endianess>",
            ),
            key(
                "",
                "<Address>0x100000000</Address><Length>4</Length><Endianess>LittleEndian</Endianess>",
            ),
            key(
                "",
                "<Address>0xfffffffc</Address><Length>8</Length><Endianess>LittleEndian</Endianess>",
            ),
            key(
                "",
                "<Address>0x0a00</Address><Length>4</Length><Endianess>LittleEndian</Endianess>",
            ),
            {
                let mut body = key(
                    "",
                    "<pAddress>Address</pAddress><Length>4</Length><Endianess>LittleEndian</Endianess>",
                );
                body.push_str("<Integer Name='Address'><Value>0x11000</Value></Integer>");
                body
            },
            key(
                "",
                "<Address>0x11000</Address><Length>4</Length><Endianess>Unknown</Endianess>",
            ),
            "<Integer Name='Other'><Value>1</Value></Integer>".to_owned(),
        ];
        bodies.push(
            key("", LAYOUT).replace("<AccessMode>WO</AccessMode>", "<AccessMode>RW</AccessMode>"),
        );
        for body in bodies {
            let (mut camera, state) = fixture(&body, Transport::GigE);
            let error = camera
                .write_action_device_key(17, Instant::now() + Duration::from_secs(1))
                .unwrap_err();
            flags(&error, false);
            let state = state.lock().unwrap();
            assert_eq!(state.reads, [] as [(u64, usize); 0]);
            assert_eq!(state.writes, [] as [(u64, Vec<u8>); 0]);
            assert_eq!(state.unbounded, 0);
        }
    }

    #[test]
    fn dependency_deadlines_and_alias_read_guard_cover_the_entire_operation() {
        let mut body = key(
            "<pIsAvailable>Gate</pIsAvailable><pIsLocked>Locked</pIsLocked><pMin>Minimum</pMin><pError>Status</pError>",
            LAYOUT,
        );
        body.push_str("<IntReg Name='Gate'><Address>0x20</Address><Length>4</Length></IntReg><IntReg Name='Locked'><Address>0x24</Address><Length>4</Length></IntReg><IntReg Name='Minimum'><Address>0x28</Address><Length>4</Length></IntReg><IntReg Name='Status'><Address>0x30</Address><Length>4</Length></IntReg>");
        let (mut camera, state) = fixture(&body, Transport::GigE);
        word(&mut state.lock().unwrap(), 0x20, 1);
        let deadline = Instant::now() + Duration::from_secs(1);
        camera.write_action_device_key(42, deadline).unwrap();
        {
            let state = state.lock().unwrap();
            assert_eq!(state.writes, [(0x0001_1000, vec![42, 0, 0, 0])]);
            assert!(
                state
                    .reads
                    .iter()
                    .all(|(address, _)| *address < 0x0001_1000)
            );
            assert_eq!(state.reads.last(), Some(&(0x30, 4)));
            assert!(state.deadlines.iter().all(|seen| *seen == deadline));
            assert_eq!(state.unbounded, 0);
        }
        let mut alias = key("<pMin>Alias</pMin>", LAYOUT);
        alias
            .push_str("<IntReg Name='Alias'><Address>0x11000</Address><Length>4</Length></IntReg>");
        let (mut camera, state) = fixture(&alias, Transport::GenTl);
        let error = camera.write_action_device_key(42, deadline).unwrap_err();
        assert_eq!(classify(&error).kind, ErrorKind::Unsupported);
        flags(&error, false);
        assert_eq!(state.lock().unwrap().reads, [] as [(u64, usize); 0]);
        assert_eq!(state.lock().unwrap().writes, [] as [(u64, Vec<u8>); 0]);
    }

    #[test]
    fn native_alignment_expansion_cannot_read_adjacent_key_bytes() {
        let (_, state) = fixture(&key("", LAYOUT), Transport::GigE);
        let mut backend = Device {
            xml: String::new(),
            state: state.clone(),
        };
        let mut attempted = false;
        let mut io = DeadlineIo {
            backend: &mut backend,
            deadline: Instant::now() + Duration::from_secs(1),
            address: 0x0001_1001,
            length: 1,
            native_gige: true,
            attempted: &mut attempted,
            failure: None,
        };
        assert_eq!(
            classify(&io.read_memory(0x0001_1000, 1).unwrap_err()).kind,
            ErrorKind::Unsupported
        );
        drop(io);
        assert!(!attempted);
        assert_eq!(state.lock().unwrap().reads, [] as [(u64, usize); 0]);
    }

    #[test]
    fn short_late_or_retired_dependency_preserves_first_cause_and_prevents_write() {
        let mut body = key("<pIsAvailable>Gate</pIsAvailable>", LAYOUT);
        body.push_str("<IntReg Name='Gate'><Address>0x20</Address><Length>4</Length></IntReg>");
        for (mode, kind) in [
            (Mode::ShortRead, ErrorKind::Incomplete),
            (Mode::LateRead, ErrorKind::Timeout),
            (Mode::RetiredRead, ErrorKind::Disconnected),
        ] {
            let (mut camera, state) = fixture(&body, Transport::GigE);
            {
                let mut state = state.lock().unwrap();
                state.mode = mode;
                word(&mut state, 0x20, 1);
            }
            let error = camera
                .write_action_device_key(17, Instant::now() + Duration::from_millis(10))
                .unwrap_err();
            assert_eq!(classify(&error).kind, kind);
            flags(&error, false);
            if mode == Mode::RetiredRead {
                assert_eq!(
                    classify(&error).details.unwrap()["controller_ownership"],
                    "retired"
                );
                assert_eq!(
                    classify(&error).hint.as_deref(),
                    Some("explicit Connect required")
                );
            }
            assert_eq!(state.lock().unwrap().writes, [] as [(u64, Vec<u8>); 0]);
        }
        let (mut camera, state) = fixture(&key("", LAYOUT), Transport::GigE);
        state.lock().unwrap().retired = true;
        let error = camera
            .write_action_device_key(17, Instant::now())
            .unwrap_err();
        assert_eq!(classify(&error).kind, ErrorKind::Disconnected);
        flags(&error, false);
        assert_eq!(state.lock().unwrap().reads, [] as [(u64, usize); 0]);
        assert_eq!(state.lock().unwrap().writes, [] as [(u64, Vec<u8>); 0]);
    }

    #[test]
    fn late_partial_or_retired_writes_are_uncertain_without_readback_or_retry() {
        for (mode, kind) in [
            (Mode::LateWrite, ErrorKind::Timeout),
            (Mode::PartialWrite, ErrorKind::Incomplete),
            (Mode::RetiredWrite, ErrorKind::Disconnected),
        ] {
            let (mut camera, state) = fixture(&key("", LAYOUT), Transport::GigE);
            state.lock().unwrap().mode = mode;
            let error = camera
                .write_action_device_key(0x1234_5678, Instant::now() + Duration::from_millis(10))
                .unwrap_err();
            assert_eq!(classify(&error).kind, kind);
            flags(&error, true);
            assert_eq!(state.lock().unwrap().reads, [] as [(u64, usize); 0]);
            assert_eq!(state.lock().unwrap().writes.len(), 1);
            if mode == Mode::RetiredWrite {
                let error = camera
                    .write_action_device_key(7, Instant::now() + Duration::from_secs(1))
                    .unwrap_err();
                assert_eq!(classify(&error).kind, ErrorKind::Disconnected);
                flags(&error, false);
                assert_eq!(state.lock().unwrap().writes.len(), 1);
            }
        }
    }

    #[test]
    fn postwrite_error_is_uncertain_and_a_prior_deadline_does_not_leak() {
        let mut body = key("<pError>Status</pError>", LAYOUT);
        body.push_str("<IntReg Name='Status'><Address>0x30</Address><Length>4</Length></IntReg>");
        let (mut camera, state) = fixture(&body, Transport::GenTl);
        word(&mut state.lock().unwrap(), 0x30, 7);
        let error = camera
            .write_action_device_key(17, Instant::now() + Duration::from_secs(1))
            .unwrap_err();
        assert_eq!(classify(&error).kind, ErrorKind::OperationFailed);
        flags(&error, true);
        assert_eq!(state.lock().unwrap().writes.len(), 1);
        let (mut camera, state) = fixture(&key("", LAYOUT), Transport::GigE);
        state.lock().unwrap().mode = Mode::LateWrite;
        let first = Instant::now() + Duration::from_millis(10);
        let error = camera.write_action_device_key(17, first).unwrap_err();
        flags(&error, true);
        state.lock().unwrap().mode = Mode::Normal;
        let second = Instant::now() + Duration::from_secs(1);
        camera.write_action_device_key(19, second).unwrap();
        assert_eq!(state.lock().unwrap().deadlines, [first, second]);
        assert_eq!(state.lock().unwrap().writes.len(), 2);
        assert_eq!(state.lock().unwrap().unbounded, 0);
    }

    #[test]
    fn key_updates_preserve_frozen_chunks_trigger_cache_and_image_epoch_without_extra_io() {
        use crate::transport::chunks::{ByteOrder, ChunkData};
        struct Frozen;
        impl RegisterIo for Frozen {
            fn read_memory(&mut self, address: u64, length: usize) -> Result<Vec<u8>> {
                if address == 0x40 && length == 4 {
                    Ok(1_u32.to_le_bytes().to_vec())
                } else {
                    Err(anyhow::anyhow!("unexpected frozen-context dependency"))
                }
            }
            fn write_memory(&mut self, _: u64, _: &[u8]) -> Result<()> {
                Err(anyhow::anyhow!("chunk preparation cannot write"))
            }
        }
        let mut body = key("", LAYOUT);
        body.push_str(concat!(
                "<Category Name='Root'><pFeature>Counter</pFeature></Category>",
                "<Integer Name='Counter'><pValue>RawCounter</pValue><pIsAvailable>ChunkEnabled</pIsAvailable></Integer>",
                "<IntReg Name='RawCounter'><Address>0</Address><Length>4</Length><Endianess>LittleEndian</Endianess><pPort>Chunk</pPort></IntReg>",
                "<IntReg Name='ChunkEnabled'><Address>0x40</Address><Length>4</Length></IntReg>",
                "<Port Name='Chunk'><ChunkID>00000012</ChunkID></Port>"
            ));
        let (mut camera, state) = fixture(&body, Transport::GigE);
        camera.streaming = true;
        camera.chunk_context = Some(
            camera
                .nodes
                .prepare_chunk_context(&mut Frozen, Duration::from_secs(1)),
        );
        camera.software_triggers = Some(vec![Some("FrameStart".into())]);
        camera.epoch.stale = 9;
        camera.epoch.pending = 3;
        camera.epoch.since_ns = Some(123);
        camera.epoch.counted_until = Some(Instant::now() + Duration::from_secs(1));
        camera.epoch.stale_until = Some(Instant::now() + Duration::from_secs(2));
        camera.epoch.last_stale = true;
        let epoch = (
            camera.epoch.stale,
            camera.epoch.pending,
            camera.epoch.since_ns,
            camera.epoch.counted_until,
            camera.epoch.stale_until,
            camera.epoch.last_stale,
        );
        let mut bytes = vec![0, 0];
        bytes.extend(7_u32.to_le_bytes());
        bytes.extend(0x12_u32.to_be_bytes());
        bytes.extend(4_u32.to_be_bytes());
        let mut chunks = ChunkData::parse(&bytes, 2, 2, ByteOrder::Big, 0).unwrap();
        chunks.payload = bytes.into();
        let before = camera
            .nodes
            .chunk_values(&chunks, camera.chunk_context.as_ref().unwrap());
        assert_eq!(before["values"]["Counter"], 7);
        assert_eq!(before["context_reads"], 1);
        let deadline = Instant::now() + Duration::from_secs(1);
        camera.write_action_device_key(17, deadline).unwrap();
        state.lock().unwrap().mode = Mode::PartialWrite;
        let error = camera.write_action_device_key(19, deadline).unwrap_err();
        assert_eq!(classify(&error).kind, ErrorKind::Incomplete);
        flags(&error, true);
        assert_eq!(
            camera.software_triggers,
            Some(vec![Some("FrameStart".into())])
        );
        assert!(camera.is_streaming());
        assert_eq!(
            (
                camera.epoch.stale,
                camera.epoch.pending,
                camera.epoch.since_ns,
                camera.epoch.counted_until,
                camera.epoch.stale_until,
                camera.epoch.last_stale
            ),
            epoch
        );
        assert_eq!(
            camera
                .nodes
                .chunk_values(&chunks, camera.chunk_context.as_ref().unwrap()),
            before
        );
        let state = state.lock().unwrap();
        assert_eq!(state.reads, [] as [(u64, usize); 0]);
        assert_eq!(
            state.writes,
            [
                (0x0001_1000, vec![17, 0, 0, 0]),
                (0x0001_1000, vec![19, 0, 0, 0])
            ]
        );
        assert_eq!(state.deadlines, [deadline, deadline]);
        assert_eq!(state.unbounded, 0);
    }

    #[test]
    fn native_span_ending_exactly_at_32_bit_boundary_is_admitted() {
        let layout =
            "<Address>0xfffffff8</Address><Length>8</Length><Endianess>LittleEndian</Endianess>";
        let (mut camera, state) = fixture(&key("", layout), Transport::GigE);
        camera
            .write_action_device_key(0x1234_5678, Instant::now() + Duration::from_secs(1))
            .unwrap();
        let state = state.lock().unwrap();
        assert_eq!(
            state.writes,
            [(0xffff_fff8, vec![0x78, 0x56, 0x34, 0x12, 0, 0, 0, 0])]
        );
        assert_eq!(state.reads, [] as [(u64, usize); 0]);
        assert_eq!(state.unbounded, 0);
    }
}
