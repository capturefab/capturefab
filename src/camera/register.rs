//! Explicit opaque-register transfers, with one deadline across all dependencies.
use super::Camera;
use crate::{
    controls::Name,
    types::{Categorized, ErrorKind, RegisterIo, control_remaining},
};
use anyhow::{Context as _, Result, ensure};
use serde_json::{Value, json};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

#[derive(Debug)]
struct SharedFailure(Arc<anyhow::Error>);
impl std::fmt::Display for SharedFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("register dependency I/O failed")
    }
}
impl std::error::Error for SharedFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref().as_ref())
    }
}
struct DeadlineIo<'a> {
    backend: &'a mut dyn crate::types::Backend,
    deadline: Instant,
    // Some GenApi metadata helpers deliberately omit unavailable entries.
    // Preserve any underlying transport cause rather than turn it into Usage.
    failure: Option<Arc<anyhow::Error>>,
}
impl DeadlineIo<'_> {
    fn track<T>(&mut self, result: Result<T>) -> Result<T> {
        result.map_err(|error| {
            let shared = Arc::new(error);
            self.failure.get_or_insert_with(|| shared.clone());
            SharedFailure(shared).into()
        })
    }
    fn check(&self) -> Result<()> {
        if let Some(error) = &self.failure {
            return Err(SharedFailure(error.clone()).into());
        }
        if let Some(error) = self.backend.terminal_error() {
            return Err(error);
        }
        control_remaining(self.deadline).map(|_| ())
    }
}
impl RegisterIo for DeadlineIo<'_> {
    fn read_memory(&mut self, address: u64, length: usize) -> Result<Vec<u8>> {
        let result = (|| {
            control_remaining(self.deadline)?;
            let bytes = self
                .backend
                .read_memory_until(address, length, self.deadline)?;
            control_remaining(self.deadline)?;
            Ok(bytes)
        })();
        self.track(result)
    }
    fn write_memory(&mut self, address: u64, data: &[u8]) -> Result<()> {
        let result = (|| {
            control_remaining(self.deadline)?;
            self.backend
                .write_memory_until(address, data, self.deadline)?;
            control_remaining(self.deadline).map(|_| ())
        })();
        self.track(result)
    }
}

pub(super) fn transfer(
    camera: &mut Camera,
    name: &Name,
    data: Option<&[u8]>,
    timeout: Duration,
) -> Result<Value> {
    if data.is_some() || !name.bank.is_empty() {
        camera.require_control()?;
    }
    name.validate()?;
    ensure!(
        !timeout.is_zero() && timeout <= Duration::from_secs(30),
        "register timeout must be greater than zero and at most 30s"
    );
    if let Some(data) = data {
        ensure!(
            !data.is_empty() && data.len() <= 65536,
            "register data must contain 1..65536 bytes"
        );
    }
    let deadline = Instant::now()
        .checked_add(timeout)
        .context("register deadline overflow")?;
    let native_gige = camera.info.transport == crate::types::Transport::GigE;
    let nodes = &camera.nodes;
    if !nodes.has(&name.name) {
        return Err(Categorized::error(
            ErrorKind::FeatureNotFound,
            format!("unknown GenICam feature {}", name.name),
        ));
    }
    if nodes.kind(&name.name) != Some("Register") {
        return Err(Categorized::error(
            ErrorKind::Unsupported,
            format!("{} is not an opaque Register feature", name.name),
        ));
    }
    let selectors = nodes
        .selectors(&name.name)
        .into_iter()
        .filter(|selector| {
            !selector.starts_with("__")
                && matches!(
                    nodes.kind(selector),
                    Some("Enumeration" | "Integer" | "Boolean")
                )
        })
        .collect::<Vec<_>>();
    let mut io = DeadlineIo {
        backend: camera.backend.as_mut(),
        deadline,
        failure: None,
    };
    let mut bank: Vec<(String, String)> = Vec::new();
    // Resolve every supplied bank before the first selector write. An explicit
    // unrelated feature must never become an accidental side effect of a read.
    for entry in &name.bank {
        control_remaining(deadline)?;
        let selector = if let Some(selector) = &entry.selector {
            if !selectors.contains(selector) {
                return Err(crate::types::Usage(format!(
                    "{} is not a declared selector of {}",
                    selector, name.name
                ))
                .into());
            }
            selector.clone()
        } else {
            let mut matching = Vec::new();
            for selector in &selectors {
                if nodes.kind(selector) == Some("Enumeration") {
                    let choices = nodes.choices(&mut io, selector)?;
                    io.check()?;
                    if choices.contains(&entry.value) {
                        matching.push(selector.clone());
                    }
                }
            }
            io.check()?;
            match (matching.as_slice(), selectors.as_slice()) {
                ([one], _) | ([], [one]) => one.clone(),
                _ => return Err(crate::types::Usage(format!("{} bank is ambiguous or has no declared selector; name SELECTOR=VALUE explicitly", name.name)).into()),
            }
        };
        if bank.iter().any(|(existing, _)| existing == &selector) {
            return Err(
                crate::types::Usage(format!("{} names {} twice", name.name, selector)).into(),
            );
        }
        bank.push((selector, entry.value.clone()));
    }
    io.check()?;
    let mut originals = Vec::new();
    let result = (|| -> Result<Value> {
        for (selector, value) in &bank {
            control_remaining(deadline)?;
            let old = nodes.get(&mut io, selector)?;
            io.check()?;
            if crate::controls::text(&old) != *value {
                originals.push((selector.clone(), old));
                nodes.set(&mut io, selector, value)?;
                io.check()?;
            }
        }
        control_remaining(deadline)?;
        let (address, length) = nodes.raw_register_layout(&mut io, &name.name, data.is_some())?;
        io.check()?;
        let bank = bank
            .iter()
            .map(|(selector, value)| json!({"selector":selector,"value":value}))
            .collect::<Vec<_>>();
        if let Some(data) = data {
            ensure!(
                data.len() == length,
                "register {} requires exactly {length} bytes; received {}",
                name.name,
                data.len()
            );
            if native_gige && (!address.is_multiple_of(4) || !length.is_multiple_of(4)) {
                return Err(Categorized::new(ErrorKind::Unsupported, "native GigE named register writes require a four-byte aligned address and length")
                    .hint("an unaligned native write needs an implicit read-modify-write; raw named buffers do not perform that read").into());
            }
            io.write_memory(address, data).with_context(|| format!("writing {} opaque bytes to {}; an unsuccessful raw write may leave a committed prefix", data.len(), name.name))?;
            Ok(json!({"feature":name.name,"bank":bank,"address":address,"written":length}))
        } else {
            let bytes = io.read_memory(address, length)?;
            ensure!(bytes.len() == length, "short opaque-register read");
            Ok(
                json!({"feature":name.name,"bank":bank,"address":address,"bytes":length,"hex":crate::types::hex(bytes)}),
            )
        }
    })();
    let mut failures = Vec::new();
    let mut first = None;
    for (selector, value) in originals.into_iter().rev() {
        io.failure = None;
        let restored = control_remaining(deadline)
            .and_then(|_| nodes.set(&mut io, &selector, &crate::controls::text(&value)))
            .and_then(|()| io.check());
        if let Err(error) = restored {
            failures.push(format!("{selector}: {error:#}"));
            first.get_or_insert(error);
        }
    }
    if let Some(first) = first {
        let context = format!(
            "could not restore register selectors: {}",
            failures.join("; ")
        );
        return match result {
            Err(error) => Err(error.context(context)),
            Ok(_) => Err(first.context(context)),
        };
    }
    match result {
        Ok(value) => {
            control_remaining(deadline)?;
            Ok(value)
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Backend, Frame, classify};
    use std::{collections::BTreeMap, sync::Mutex};

    const XML: &str = r#"<RegisterDescription>
      <Category Name="Root"><pFeature>Buffer</pFeature><pFeature>Bank</pFeature></Category>
      <Enumeration Name="Bank"><EnumEntry Name="A"><Value>0</Value></EnumEntry><EnumEntry Name="B"><Value>1</Value></EnumEntry><pValue>BankReg</pValue><pSelected>Buffer</pSelected></Enumeration>
      <IntReg Name="BankReg"><Address>0</Address><Length>4</Length><AccessMode>RW</AccessMode><Endianess>LittleEndian</Endianess></IntReg>
      <Integer Name="Width"><Value>4</Value></Integer>
      <Integer Name="Length"><pValue>LengthReg</pValue></Integer>
      <IntReg Name="LengthReg"><Address>4</Address><Length>4</Length><AccessMode>RO</AccessMode><Endianess>LittleEndian</Endianess></IntReg>
      <IntSwissKnife Name="Offset"><pVariable Name="B">Bank</pVariable><Formula>B * 8192</Formula></IntSwissKnife>
      <Register Name="Buffer"><Address>0x1000</Address><pAddress>Offset</pAddress><pLength>Length</pLength><AccessMode>RW</AccessMode><pPort>Device</pPort></Register>
      <Port Name="Device"/>
      </RegisterDescription>"#;
    #[derive(Default)]
    struct State {
        bytes: BTreeMap<u64, u8>,
        reads: Vec<(u64, usize)>,
        writes: Vec<(u64, Vec<u8>)>,
        deadlines: Vec<Instant>,
        body_late: bool,
        partial_failure: bool,
        normal_calls: usize,
        gate_failure: bool,
    }
    struct Device(Arc<Mutex<State>>);
    impl RegisterIo for Device {
        fn read_memory(&mut self, _: u64, _: usize) -> Result<Vec<u8>> {
            self.0
                .lock()
                .map_err(|_| anyhow::anyhow!("fixture mutex poisoned"))?
                .normal_calls += 1;
            anyhow::bail!("unbounded register read")
        }
        fn write_memory(&mut self, _: u64, _: &[u8]) -> Result<()> {
            self.0
                .lock()
                .map_err(|_| anyhow::anyhow!("fixture mutex poisoned"))?
                .normal_calls += 1;
            anyhow::bail!("unbounded register write")
        }
        fn read_memory_until(
            &mut self,
            address: u64,
            length: usize,
            deadline: Instant,
        ) -> Result<Vec<u8>> {
            control_remaining(deadline)?;
            let mut state = self
                .0
                .lock()
                .map_err(|_| anyhow::anyhow!("fixture mutex poisoned"))?;
            state.deadlines.push(deadline);
            state.reads.push((address, length));
            if address == 8 && state.gate_failure {
                return Err(
                    Categorized::new(ErrorKind::Disconnected, "first hidden gate failure")
                        .details(json!({"controller_ownership":"retired"}))
                        .into(),
                );
            }
            let late = address >= 0x1000 && state.body_late;
            let bytes = (0..length)
                .map(|index| {
                    state
                        .bytes
                        .get(&(address + index as u64))
                        .copied()
                        .unwrap_or(0)
                })
                .collect();
            drop(state);
            if late {
                std::thread::sleep(
                    deadline.saturating_duration_since(Instant::now()) + Duration::from_millis(2),
                );
            }
            // Deliberately return a late success: the caller must reject it.
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
                .0
                .lock()
                .map_err(|_| anyhow::anyhow!("fixture mutex poisoned"))?;
            state.deadlines.push(deadline);
            state.writes.push((address, data.to_vec()));
            if state.partial_failure && address >= 0x1000 {
                state.bytes.insert(address, data[0]);
                return Err(Categorized::new(
                    ErrorKind::Disconnected,
                    "controller ownership retired",
                )
                .details(json!({"controller_ownership":"retired"}))
                .into());
            }
            if state.partial_failure && address == 0 && data == [0, 0, 0, 0] {
                return Err(Categorized::error(
                    ErrorKind::Timeout,
                    "selector restore timed out",
                ));
            }
            for (offset, byte) in data.iter().enumerate() {
                state.bytes.insert(address + offset as u64, *byte);
            }
            Ok(())
        }
    }
    impl Backend for Device {
        fn xml(&mut self) -> Result<String> {
            Ok(XML.into())
        }
        fn start(&mut self, _: usize) -> Result<()> {
            Ok(())
        }
        fn next_frame(&mut self, _: Duration) -> Result<Frame> {
            anyhow::bail!("no frame")
        }
        fn stop(&mut self) -> Result<()> {
            Ok(())
        }
    }
    fn fixture() -> (Camera, Arc<Mutex<State>>) {
        let state = Arc::new(Mutex::new(State::default()));
        {
            let mut value = state.lock().unwrap();
            for (index, byte) in 8192u32.to_le_bytes().into_iter().enumerate() {
                value.bytes.insert(4 + index as u64, byte);
            }
            for index in 0..8192 {
                value
                    .bytes
                    .insert(0x3000 + index, u8::try_from(index % 256).unwrap());
            }
        }
        let camera = Camera::from_backend(
            crate::transport::simulator::info(),
            Box::new(Device(state.clone())),
        )
        .unwrap();
        (camera, state)
    }
    #[test]
    fn banked_register_uses_one_deadline_and_exact_opaque_bytes_then_restores() {
        let (mut camera, state) = fixture();
        let name = Name::parse("Buffer[Bank=B]").unwrap();
        let value = camera
            .read_register_feature(&name, Duration::from_secs(1))
            .unwrap();
        assert_eq!(value["bank"], json!([{"selector":"Bank","value":"B"}]));
        assert_eq!(value["address"], 0x3000);
        assert_eq!(value["bytes"], 8192);
        let bytes = crate::types::decode_hex(value["hex"].as_str().unwrap(), 65536).unwrap();
        assert_eq!(&bytes[..4], &[0, 1, 2, 3]);
        assert_eq!(&bytes[128..131], &[128, 129, 130]);
        assert_eq!(bytes[255], 255);
        let data = vec![0x80; 8192];
        camera
            .write_register_feature(&name, &data, Duration::from_secs(1))
            .unwrap();
        let state = state.lock().unwrap();
        assert_eq!(state.normal_calls, 0);
        assert_eq!(state.bytes[&0], 0);
        assert_eq!(
            state
                .reads
                .iter()
                .filter(|&&(address, _)| address == 0x3000)
                .count(),
            1
        );
        assert_eq!(
            state
                .reads
                .iter()
                .filter(|&&(address, _)| address == 4)
                .count(),
            2,
            "length resolved once per operation"
        );
        assert_eq!(
            state
                .writes
                .iter()
                .filter(|(address, _)| *address == 0x3000)
                .count(),
            1
        );
        let first = state.deadlines[0];
        let split = state
            .deadlines
            .iter()
            .position(|&deadline| deadline != first)
            .unwrap();
        assert!(
            state.deadlines[..split]
                .iter()
                .all(|&deadline| deadline == first)
        );
        assert!(
            state.deadlines[split..]
                .iter()
                .all(|&deadline| deadline == state.deadlines[split])
        );
    }
    #[test]
    fn unrelated_duplicate_and_wrong_length_requests_do_not_touch_buffer() {
        for input in ["Buffer[Width=4]", "Buffer[Bank=A,Bank=B]"] {
            let (mut camera, state) = fixture();
            let error = Name::parse(input)
                .and_then(|name| camera.read_register_feature(&name, Duration::from_secs(1)))
                .unwrap_err();
            assert_ne!(format!("{error:#}"), "");
            let state = state.lock().unwrap();
            assert_eq!(state.writes, [] as [(u64, Vec<u8>); 0]);
            assert_eq!(state.reads, [] as [(u64, usize); 0]);
        }
        let (mut camera, state) = fixture();
        let error = camera
            .write_register_feature(
                &Name::parse("Buffer[Bank=B]").unwrap(),
                &[1, 2],
                Duration::from_secs(1),
            )
            .unwrap_err();
        assert!(format!("{error:#}").contains("exactly 8192"));
        let state = state.lock().unwrap();
        assert!(state.writes.iter().all(|(address, _)| *address < 0x1000));
        assert_eq!(state.bytes[&0], 0);
    }
    #[test]
    fn late_body_success_is_rejected_without_starting_selector_restoration() {
        let (mut camera, state) = fixture();
        state.lock().unwrap().body_late = true;
        let error = camera
            .read_register_feature(
                &Name::parse("Buffer[Bank=B]").unwrap(),
                Duration::from_millis(20),
            )
            .unwrap_err();
        assert_eq!(classify(&error).kind, ErrorKind::Timeout);
        let state = state.lock().unwrap();
        assert_eq!(
            state.writes.len(),
            1,
            "no restoration I/O begins after cutoff"
        );
        assert_eq!(state.bytes[&0], 1);
        assert_eq!(state.normal_calls, 0);
    }
    #[test]
    fn partial_write_keeps_typed_primary_cause_without_backup_or_rollback() {
        let (mut camera, state) = fixture();
        state.lock().unwrap().partial_failure = true;
        let error = camera
            .write_register_feature(
                &Name::parse("Buffer[Bank=B]").unwrap(),
                &[0xff; 8192],
                Duration::from_secs(1),
            )
            .unwrap_err();
        assert_eq!(classify(&error).kind, ErrorKind::Disconnected);
        assert!(crate::transport::gige::ownership_retired(&error));
        assert!(format!("{error:#}").contains("restore"));
        let state = state.lock().unwrap();
        assert_eq!(state.bytes[&0x3000], 255);
        assert_eq!(state.bytes[&0x3001], 1);
        assert!(
            state.reads.iter().all(|(address, _)| *address < 0x1000),
            "no body backup/readback"
        );
        assert_eq!(
            state
                .writes
                .iter()
                .filter(|(address, _)| *address >= 0x1000)
                .count(),
            1
        );
    }
    #[test]
    fn shorthand_bank_keeps_transport_error_hidden_by_entry_availability() {
        let (mut camera, state) = fixture();
        camera.nodes=crate::genicam::NodeMap::parse(&XML.replace("<EnumEntry Name=\"B\"><Value>1</Value>","<EnumEntry Name=\"B\"><Value>1</Value><pIsAvailable>Gate</pIsAvailable>")
            .replace("<Port Name=\"Device\"/>","<IntReg Name=\"Gate\"><Address>8</Address><Length>4</Length><AccessMode>RO</AccessMode></IntReg><Enumeration Name=\"Other\"><EnumEntry Name=\"C\"><Value>0</Value></EnumEntry><Value>0</Value><pSelected>Buffer</pSelected></Enumeration><Port Name=\"Device\"/>" )).unwrap();
        state.lock().unwrap().gate_failure = true;
        let error = camera
            .read_register_feature(&Name::parse("Buffer[B]").unwrap(), Duration::from_secs(1))
            .unwrap_err();
        assert_eq!(classify(&error).kind, ErrorKind::Disconnected);
        assert!(crate::transport::gige::ownership_retired(&error));
        assert!(format!("{error:#}").contains("first hidden gate failure"));
        let state = state.lock().unwrap();
        assert_eq!(state.writes, [] as [(u64, Vec<u8>); 0]);
        assert_eq!(state.reads, [(8, 4)]);
    }
    #[test]
    fn native_unaligned_opaque_write_refuses_body_read_modify_write() {
        let (mut camera, state) = fixture();
        camera.info.transport = crate::types::Transport::GigE;
        camera.nodes = crate::genicam::NodeMap::parse(
            &XML.replace("<Address>0x1000</Address>", "<Address>0x1001</Address>"),
        )
        .unwrap();
        let error = camera
            .write_register_feature(
                &Name::parse("Buffer").unwrap(),
                &[0; 8192],
                Duration::from_secs(1),
            )
            .unwrap_err();
        assert_eq!(classify(&error).kind, ErrorKind::Unsupported);
        let state = state.lock().unwrap();
        assert_eq!(state.writes, [] as [(u64, Vec<u8>); 0]);
        assert!(state.reads.iter().all(|(address, _)| *address < 0x1000));
    }
}
