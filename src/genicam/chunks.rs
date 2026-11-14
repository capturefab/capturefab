//! Read-only interpretation of retained camera chunks through their XML ports.
//! Chunk IDs are hexadecimal strings in the `GenApi` schema, not decimal values.
//! <https://www.emva.org/wp-content/uploads/GenICam_Standard_v2_1_1.pdf>
use super::{FEATURE_KINDS, Node, NodeMap, RegisterIo, unsigned_integer};
use crate::transport::chunks::ChunkData;
use anyhow::{Context as _, Result, ensure};
use serde::Serialize as _;
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    io,
    time::{Duration, Instant},
};

const MAX_CANDIDATES: usize = 256;
const MAX_READS: usize = 32;
const MAX_BYTES: usize = 64 * 1024;
const MAX_OUTPUT: usize = 64 * 1024;
const MAX_NAME: usize = 256;
const MAX_ERROR: usize = 512;

#[derive(Debug, Clone, Default)]
pub(super) struct Plan {
    candidates: Vec<String>,
    dependencies: Vec<String>,
    truncated: bool,
}
impl Plan {
    pub(super) fn new(map: &NodeMap) -> Self {
        let mut reverse: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        let mut dependent = BTreeSet::new();
        let mut pending = VecDeque::new();
        for (name, node) in &map.nodes {
            if node.kind == "Port"
                && (node.prop("ChunkID").is_some() || node.prop("pChunkID").is_some())
            {
                pending.push_back(name.as_str());
            }
            for link in links(node) {
                reverse.entry(link).or_default().push(name);
            }
        }
        while let Some(name) = pending.pop_front() {
            if dependent.insert(name)
                && let Some(parents) = reverse.get(name)
            {
                pending.extend(parents.iter().copied());
            }
        }
        let mut plan = Self::default();
        for name in map.feature_names() {
            if dependent.contains(name) && !matches!(map.kind(name), Some("Command" | "Register")) {
                if name.len() > MAX_NAME || plan.candidates.len() >= MAX_CANDIDATES {
                    plan.truncated = true;
                } else {
                    plan.candidates.push(name.to_owned());
                }
            }
        }
        let mut visited = BTreeSet::new();
        let mut pending = plan.candidates.clone();
        while let Some(name) = pending.pop() {
            if !visited.insert(name.clone()) {
                continue;
            }
            if let Some(node) = map.nodes.get(&name) {
                pending.extend(links(node).into_iter().map(str::to_owned));
                if !dependent.contains(name.as_str())
                    && (FEATURE_KINDS.contains(&node.kind.as_str()) || node.kind == "EnumEntry")
                    && !matches!(node.kind.as_str(), "Command" | "Register")
                {
                    plan.dependencies.push(name);
                }
            }
        }
        plan.dependencies.sort();
        plan
    }
}

/// Read dependencies exclude category/selector associations and write-only hooks.
fn links(node: &Node) -> Vec<&str> {
    let mut links = Vec::new();
    for property in &node.props {
        if ((property.tag.starts_with('p')
            && !matches!(
                property.tag.as_str(),
                "pFeature"
                    | "pSelected"
                    | "pAlias"
                    | "pCastAlias"
                    | "pInvalidator"
                    | "pBlockPolling"
                    | "pError"
                    | "pValueCopy"
                    | "pIsLocked"
                    | "pCommandValue"
            ))
            || property.tag == "IntSwissKnife")
            && !property.text.is_empty()
        {
            links.push(property.text.as_str());
        }
        if let Some(offset) = property.attrs.get("pOffset") {
            links.push(offset);
        }
    }
    links
}

/// Device bytes used by chunk formulas and availability gates, frozen before acquisition.
#[derive(Debug, Clone, Default)]
pub(crate) struct ChunkContext {
    ranges: BTreeMap<(u64, usize), Vec<u8>>,
    reads: usize,
    bytes: usize,
    issue: Option<String>,
}
impl ChunkContext {
    fn read(&self, address: u64, length: usize) -> Option<Vec<u8>> {
        for (&(start, _), bytes) in &self.ranges {
            let Some(offset) = address
                .checked_sub(start)
                .and_then(|offset| usize::try_from(offset).ok())
            else {
                continue;
            };
            let Some(end) = offset.checked_add(length) else {
                continue;
            };
            if let Some(value) = bytes.get(offset..end) {
                return Some(value.to_vec());
            }
        }
        None
    }
}

struct Prepare<'a> {
    device: &'a mut dyn RegisterIo,
    context: ChunkContext,
    deadline: Instant,
}
impl RegisterIo for Prepare<'_> {
    fn read_memory(&mut self, address: u64, length: usize) -> Result<Vec<u8>> {
        if let Some(bytes) = self.context.read(address, length) {
            return Ok(bytes);
        }
        ensure!(
            self.context.issue.is_none(),
            "chunk dependency snapshot stopped after an earlier failure"
        );
        let unavailable = if Instant::now() >= self.deadline {
            Some("chunk dependency snapshot deadline expired")
        } else if self.context.reads >= MAX_READS
            || length > MAX_BYTES.saturating_sub(self.context.bytes)
        {
            Some("chunk dependency snapshot exceeds 32 reads or 64 KiB")
        } else {
            None
        };
        if let Some(issue) = unavailable {
            self.context.issue = Some(issue.into());
            anyhow::bail!("{issue}");
        }
        self.context.reads += 1;
        match self.device.read_memory(address, length) {
            Ok(bytes) if bytes.len() == length => {
                self.context.bytes += length;
                self.context.ranges.insert((address, length), bytes.clone());
                Ok(bytes)
            }
            result => {
                let issue = match result {
                    Ok(_) => "short chunk dependency register read".into(),
                    Err(error) => short(&format!("chunk dependency read failed: {error:#}")),
                };
                self.context.issue = Some(issue.clone());
                anyhow::bail!("{issue}")
            }
        }
    }
    fn write_memory(&mut self, _: u64, _: &[u8]) -> Result<()> {
        anyhow::bail!("chunk dependency snapshots are read-only")
    }
}

struct Snapshot<'a> {
    chunks: &'a ChunkData,
    context: &'a ChunkContext,
}
impl RegisterIo for Snapshot<'_> {
    fn read_memory(&mut self, address: u64, length: usize) -> Result<Vec<u8>> {
        self.context.read(address, length).with_context(|| {
            self.context.issue.as_ref().map_or_else(
                || "live dependency was not captured before acquisition".into(),
                |issue| format!("live dependency unavailable: {issue}"),
            )
        })
    }
    fn write_memory(&mut self, _: u64, _: &[u8]) -> Result<()> {
        anyhow::bail!("retained camera chunks are read-only")
    }
    fn read_chunk_memory(&mut self, chunk_id: u64, address: u64, length: usize) -> Result<Vec<u8>> {
        ensure!(
            length <= MAX_BYTES,
            "chunk register exceeds the 64 KiB interpretation limit"
        );
        ensure!(
            self.chunks.payload.len() == self.chunks.payload_bytes,
            "retained chunk payload bytes are missing"
        );
        let mut matches = self
            .chunks
            .entries
            .iter()
            .filter(|entry| entry.id == chunk_id);
        let entry = matches.next().with_context(|| {
            self.chunks.descriptor_error.as_ref().map_or_else(
                || "XML-declared chunk is absent from this frame".to_owned(),
                |issue| format!("chunk descriptor unavailable: {issue}"),
            )
        })?;
        ensure!(
            matches.next().is_none(),
            "XML-declared chunk ID is ambiguous in this frame"
        );
        ensure!(
            entry
                .offset
                .checked_add(entry.length)
                .is_some_and(|end| end <= self.chunks.payload.len()),
            "chunk descriptor exceeds retained payload"
        );
        let offset = usize::try_from(address).context("chunk address exceeds host address size")?;
        let end = offset
            .checked_add(length)
            .context("chunk register range overflow")?;
        ensure!(
            end <= entry.length,
            "chunk register exceeds its declared chunk boundary"
        );
        let start = entry
            .offset
            .checked_add(offset)
            .context("chunk address overflow")?;
        let end = start
            .checked_add(length)
            .context("chunk address overflow")?;
        Ok(self
            .chunks
            .payload
            .get(start..end)
            .context("chunk bytes exceed retained payload")?
            .to_vec())
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) enum ReadTarget {
    Device(u64),
    Chunk(u64, u64),
}
impl ReadTarget {
    pub(super) fn read(self, io: &mut dyn RegisterIo, length: usize) -> Result<Vec<u8>> {
        match self {
            Self::Device(address) => io.read_memory(address, length),
            Self::Chunk(id, address) => io.read_chunk_memory(id, address, length),
        }
    }
}

impl NodeMap {
    pub(crate) const fn has_chunk_features(&self) -> bool {
        !self.chunk_plan.candidates.is_empty()
    }

    /// Freeze only device-backed dependencies used by XML-declared chunk features.
    /// A failed transport call stops further device reads; existing transport deadlines
    /// bound that one call, while `timeout` bounds subsequent dependency work.
    pub(crate) fn prepare_chunk_context(
        &self,
        io: &mut dyn RegisterIo,
        timeout: Duration,
    ) -> ChunkContext {
        let deadline = Instant::now()
            .checked_add(timeout)
            .unwrap_or_else(Instant::now);
        let mut prepare = Prepare {
            device: io,
            context: ChunkContext::default(),
            deadline,
        };
        for dependency in &self.chunk_plan.dependencies {
            drop(self.get(&mut prepare, dependency));
            if prepare.context.issue.is_some() {
                break;
            }
        }
        prepare.context
    }

    /// Interpret the retained frame without accessing the camera. Errors are per
    /// feature, so missing/disabled chunks never destroy the original payload.
    pub(crate) fn chunk_values(&self, chunks: &ChunkData, context: &ChunkContext) -> Value {
        let mut snapshot = Snapshot { chunks, context };
        let mut values = Map::new();
        let mut errors = Map::new();
        let mut units = Map::new();
        let mut truncated = self.chunk_plan.truncated;
        // Reserve more than the outer document's fixed fields require.
        let mut remaining = MAX_OUTPUT - 128;
        for name in &self.chunk_plan.candidates {
            let (value, error) = match self.get(&mut snapshot, name) {
                Ok(value) => (value, false),
                Err(error) => (json!(short(&format!("{error:#}"))), true),
            };
            let unit = (!error)
                .then(|| self.unit(name))
                .flatten()
                .filter(|unit| !unit.is_empty());
            let mut budget = Count {
                bytes: if unit.is_some() { 4 } else { 2 },
                maximum: remaining,
            };
            if name
                .serialize(&mut serde_json::Serializer::new(&mut budget))
                .is_err()
                || value
                    .serialize(&mut serde_json::Serializer::new(&mut budget))
                    .is_err()
                || unit.is_some_and(|unit| {
                    name.serialize(&mut serde_json::Serializer::new(&mut budget))
                        .is_err()
                        || unit
                            .serialize(&mut serde_json::Serializer::new(&mut budget))
                            .is_err()
                })
            {
                truncated = true;
                break;
            }
            remaining = remaining.saturating_sub(budget.bytes);
            if error {
                errors.insert(name.clone(), value);
            } else {
                if let Some(unit) = unit {
                    units.insert(name.clone(), json!(unit));
                }
                values.insert(name.clone(), value);
            }
        }
        json!({"values": values,"units": units,"errors": errors,"truncated": truncated,
               "context_reads": context.reads,"context_bytes": context.bytes})
    }

    pub(super) fn read_register(
        &self,
        io: &mut dyn RegisterIo,
        n: &Node,
        depth: usize,
    ) -> Result<(ReadTarget, usize)> {
        if let Some(port_name) = n.prop("pPort") {
            let port = self.node(port_name, depth + 1)?;
            ensure!(port.kind == "Port", "register target is not a Port");
            ensure!(
                port.prop("EventID").is_none() && port.prop("pEventID").is_none(),
                "event ports are not supported for register access"
            );
            let chunk_id = if let Some(id) = port.prop("ChunkID") {
                ensure!(
                    port.prop("pChunkID").is_none(),
                    "chunk port has conflicting ID declarations"
                );
                let id = id
                    .strip_prefix("0x")
                    .or_else(|| id.strip_prefix("0X"))
                    .unwrap_or(id);
                ensure!(
                    !id.is_empty()
                        && id.len() <= 16
                        && id.bytes().all(|byte| byte.is_ascii_hexdigit()),
                    "invalid hexadecimal XML ChunkID"
                );
                Some(u64::from_str_radix(id, 16).context("invalid XML ChunkID")?)
            } else if let Some(link) = port.prop("pChunkID") {
                Some(unsigned_integer(&self.numeric_value(
                    io,
                    link,
                    depth + 1,
                )?)?)
            } else {
                None
            };
            if let Some(id) = chunk_id {
                self.available(io, port, depth + 1)?;
                let (address, length) = self.register_layout(io, n, depth)?;
                return Ok((ReadTarget::Chunk(id, address), length));
            }
        }
        let (address, length) = self.register(io, n, depth)?;
        Ok((ReadTarget::Device(address), length))
    }
}

fn short(text: &str) -> String {
    let mut end = text.len().min(MAX_ERROR);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.get(..end).unwrap_or_default().to_owned()
}
struct Count {
    bytes: usize,
    maximum: usize,
}
impl io::Write for Count {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.bytes = self.bytes.saturating_add(bytes.len());
        if self.bytes > self.maximum {
            return Err(io::Error::other(
                "chunk interpretation exceeds output limit",
            ));
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::chunks::ByteOrder;
    use std::fmt::Write as _;

    #[derive(Default)]
    struct Device {
        memory: BTreeMap<(u64, usize), Vec<u8>>,
        reads: usize,
        writes: usize,
        fail: bool,
        delay: Duration,
    }
    impl RegisterIo for Device {
        fn read_memory(&mut self, address: u64, length: usize) -> Result<Vec<u8>> {
            self.reads += 1;
            std::thread::sleep(self.delay);
            ensure!(!self.fail, "device offline");
            Ok(self
                .memory
                .get(&(address, length))
                .cloned()
                .unwrap_or_else(|| vec![1; length]))
        }
        fn write_memory(&mut self, _: u64, _: &[u8]) -> Result<()> {
            self.writes += 1;
            Ok(())
        }
    }
    fn parse_map(body: &str) -> NodeMap {
        NodeMap::parse(&format!(
            "<RegisterDescription>{body}</RegisterDescription>"
        ))
        .unwrap()
    }
    fn payload(chunks: &[(u32, &[u8])]) -> ChunkData {
        let mut payload = vec![7, 8];
        for &(id, bytes) in chunks {
            payload.extend_from_slice(bytes);
            payload.extend_from_slice(&id.to_be_bytes());
            payload.extend_from_slice(&u32::try_from(bytes.len()).unwrap().to_be_bytes());
        }
        let mut chunks = ChunkData::parse(&payload, 2, 2, ByteOrder::Big, 0).unwrap();
        chunks.payload = payload.into();
        chunks
    }

    #[test]
    fn producer_indexed_64_bit_ids_decode_and_duplicates_remain_explicit() {
        use crate::transport::chunks::{Chunk, ImageLayout};
        let map = parse_map(
            "<Integer Name='Measured'><pValue>Raw</pValue></Integer><IntReg Name='Raw'><pPort>Chunk</pPort><Address>0</Address><Length>8</Length><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess></IntReg><Port Name='Chunk'><ChunkID>FFFFFFFFFFFFFFFD</ChunkID></Port>",
        );
        let mut bytes = vec![0xaa, 0xbb, 1, 2, 3];
        bytes.extend(u64::MAX.to_le_bytes());
        let entry = Chunk {
            id: u64::MAX - 2,
            offset: 5,
            length: 8,
        };
        let mut chunks = ChunkData::indexed(
            &bytes,
            3,
            ImageLayout {
                pixel_encoding: None,
                offset: 2,
                pfnc_code: Some(crate::types::MONO8),
                width: 3,
                height: 1,
                row_bytes: 3,
                row_stride: 3,
                padding_y: 0,
                byte_order: None,
                continuous_packing: false,
            },
            None,
            vec![entry],
        )
        .unwrap();
        chunks.payload = bytes.clone().into();
        let context = ChunkContext::default();
        let values = map.chunk_values(&chunks, &context);
        assert_eq!(values["values"]["Measured"].as_u64(), Some(u64::MAX));
        chunks.entries.push(entry);
        let values = map.chunk_values(&chunks, &context);
        assert!(
            values["errors"]["Measured"]
                .as_str()
                .unwrap()
                .contains("ambiguous")
        );
        assert_eq!(chunks.payload.as_ref(), bytes);
    }

    #[test]
    fn basler_hexadecimal_chunk_ids_decode_exact_timestamp_and_counter() {
        let map = NodeMap::parse(include_str!(
            "../../tests/fixtures/genicam/chunks-basler.xml"
        ))
        .unwrap();
        let mut device = Device::default();
        assert!(map.has_chunk_features());
        let context = map.prepare_chunk_context(&mut device, Duration::from_secs(1));
        assert_eq!(device.reads, 0);
        let chunks = payload(&[
            (0xc916_13db, &u64::MAX.to_le_bytes()),
            (0x8c0f_1cd8, &123_u32.to_le_bytes()),
        ]);
        let before = chunks.payload.to_vec();
        let values = map.chunk_values(&chunks, &context);
        assert_eq!(values["values"]["ChunkTimestamp"], u64::MAX);
        assert!(
            !values["units"]
                .as_object()
                .unwrap()
                .contains_key("ChunkTimestamp")
        );
        assert_eq!(values["values"]["ChunkFramecounter"], 123);
        assert_eq!(values["errors"], json!({}));
        assert_eq!(chunks.payload.as_ref(), before);
        assert_eq!(device.reads, 0);
        assert_eq!(device.writes, 0);
        let error = map.get(&mut device, "ChunkTimestamp").unwrap_err();
        assert_eq!(
            crate::types::classify(&error).kind,
            crate::types::ErrorKind::Unsupported
        );
    }

    #[test]
    fn flir_live_presence_bits_are_deduplicated_and_frozen_before_frames() {
        let map =
            NodeMap::parse(include_str!("../../tests/fixtures/genicam/chunks-flir.xml")).unwrap();
        let mut device = Device::default();
        device.memory.insert(
            (0x0015_0050, 4),
            ((1_u32 << 2) | (1 << 13)).to_be_bytes().to_vec(),
        );
        let context = map.prepare_chunk_context(&mut device, Duration::from_secs(1));
        assert_eq!(device.reads, 1);
        device.memory.clear();
        device.fail = true;
        let chunks = payload(&[
            (0x0504_0003, &u64::MAX.to_le_bytes()),
            (0x0508_000e, &99_u64.to_le_bytes()),
        ]);
        let values = map.chunk_values(&chunks, &context);
        assert_eq!(values["values"]["ChunkFrameID"], u64::MAX);
        assert_eq!(values["values"]["ChunkTimestamp"], 99);
        assert_eq!(values["errors"], json!({}));
        assert_eq!(device.reads, 1);
        assert_eq!(values["context_reads"], 1);
        assert_eq!(values["context_bytes"], 4);
        let values = map.chunk_values(&chunks, &ChunkContext::default());
        assert!(
            values["errors"]["ChunkFrameID"]
                .as_str()
                .unwrap()
                .contains("not captured")
        );
        let mut device = Device::default();
        device.memory.insert((0x0015_0050, 4), vec![0; 4]);
        let context = map.prepare_chunk_context(&mut device, Duration::from_secs(1));
        let values = map.chunk_values(&chunks, &context);
        assert_eq!(values["values"], json!({}));
        assert!(
            values["errors"]["ChunkTimestamp"]
                .as_str()
                .unwrap()
                .contains("not implemented")
        );
    }

    #[test]
    fn snapshot_handles_masks_formulas_strings_and_chunk_relative_offsets() {
        let map = parse_map(concat!(
            "<Category Name='Root'><pFeature>Scaled</pFeature><pFeature>Label</pFeature></Category>",
            "<Float Name='Scaled'><pValue>Convert</pValue><Unit>mm</Unit></Float><SwissKnife Name='Convert'><pVariable Name='X'>Raw</pVariable><Formula>X * 0.25</Formula></SwissKnife>",
            "<MaskedIntReg Name='Raw'><pPort>Chunk</pPort><Address>1</Address><Length>2</Length><LSB>7</LSB><MSB>0</MSB><Endianess>BigEndian</Endianess></MaskedIntReg>",
            "<StringReg Name='Label'><pPort>Chunk</pPort><Address>3</Address><Length>4</Length></StringReg><Port Name='Chunk'><ChunkID>00000012</ChunkID></Port>"
        ));
        let chunks = payload(&[(0x12, &[9, 8, 7, b'a', b'b', 0, b'c'])]);
        let values = map.chunk_values(&chunks, &ChunkContext::default());
        assert_eq!(values["values"]["Scaled"], 2.0);
        assert_eq!(values["units"]["Scaled"], "mm");
        assert_eq!(values["values"]["Label"], "ab");
        assert_eq!(values["errors"], json!({}));
    }

    #[test]
    fn dynamic_chunk_ids_and_port_availability_follow_xml_without_device_access() {
        let map = parse_map(concat!(
            "<Category Name='Root'><pFeature>Counter</pFeature></Category>",
            "<Integer Name='Counter'><pValue>Raw</pValue></Integer>",
            "<Integer Name='Id'><Value>18</Value></Integer><Integer Name='Available'><Value>0</Value></Integer>",
            "<Port Name='Chunk'><pChunkID>Id</pChunkID><pIsAvailable>Available</pIsAvailable></Port>",
            "<IntReg Name='Raw'><Address>0</Address><Length>4</Length><pPort>Chunk</pPort></IntReg>"
        ));
        let chunks = payload(&[(0x12, &321_u32.to_le_bytes())]);
        let mut device = Device::default();
        let context = map.prepare_chunk_context(&mut device, Duration::from_secs(1));
        let values = map.chunk_values(&chunks, &context);
        assert!(
            values["errors"]["Counter"]
                .as_str()
                .unwrap()
                .contains("unavailable")
        );
        map.set(&mut device, "Available", "1").unwrap();
        let context = map.prepare_chunk_context(&mut device, Duration::from_secs(1));
        assert_eq!(
            map.chunk_values(&chunks, &context)["values"]["Counter"],
            321
        );
        map.set(&mut device, "Id", "19").unwrap();
        let context = map.prepare_chunk_context(&mut device, Duration::from_secs(1));
        let values = map.chunk_values(&chunks, &context);
        assert!(
            values["errors"]["Counter"]
                .as_str()
                .unwrap()
                .contains("absent")
        );
        assert_eq!(device.reads, 0);
        assert_eq!(device.writes, 0);
    }

    #[test]
    fn missing_ambiguous_short_and_corrupt_chunks_report_errors_without_live_reads() {
        let map = NodeMap::parse(include_str!(
            "../../tests/fixtures/genicam/chunks-basler.xml"
        ))
        .unwrap();
        let context = ChunkContext::default();
        for chunks in [
            payload(&[]),
            payload(&[(0xc916_13db, &[1; 7])]),
            payload(&[(0xc916_13db, &[1; 8]), (0xc916_13db, &[1; 8])]),
        ] {
            let values = map.chunk_values(&chunks, &context);
            assert!(values["errors"]["ChunkTimestamp"].is_string());
        }
        let mut chunks = payload(&[(0xc916_13db, &[1; 8])]);
        chunks.entries[0].length = usize::MAX;
        let values = map.chunk_values(&chunks, &context);
        assert!(
            values["errors"]["ChunkTimestamp"]
                .as_str()
                .unwrap()
                .contains("descriptor")
        );
        let mut chunks = payload(&[(0xc916_13db, &[1; 8])]);
        chunks.payload = crate::framebuf::FrameBuf::default();
        let values = map.chunk_values(&chunks, &context);
        assert!(
            values["errors"]["ChunkTimestamp"]
                .as_str()
                .unwrap()
                .contains("missing")
        );
    }

    #[test]
    fn chunk_and_event_registers_never_write_device_memory() {
        for declaration in ["<ChunkID>00000012</ChunkID>", "<EventID>12</EventID>"] {
            let map = parse_map(&format!(
                "<IntReg Name='Value'><Address>0</Address><Length>4</Length><AccessMode>RW</AccessMode><pPort>P</pPort></IntReg><Port Name='P'>{declaration}</Port>"
            ));
            let mut device = Device::default();
            map.set(&mut device, "Value", "42").unwrap_err();
            assert_eq!(device.writes, 0);
            assert_eq!(device.reads, 0);
        }
    }

    #[test]
    fn preparation_bounds_unique_device_reads_and_stops_after_failure_or_deadline() {
        let mut body = "<Port Name='Device'/><Port Name='Chunk'><ChunkID>00000012</ChunkID></Port><IntReg Name='Raw'><Address>0</Address><Length>4</Length><pPort>Chunk</pPort></IntReg>".to_owned();
        for index in 0..40 {
            write!(body,"<Integer Name='F{index}'><pIsImplemented>G{index}</pIsImplemented><pValue>Raw</pValue></Integer><IntReg Name='G{index}'><Address>{}</Address><Length>4</Length><pPort>Device</pPort></IntReg>",index*4).unwrap();
        }
        let map = parse_map(&body);
        let mut device = Device::default();
        let context = map.prepare_chunk_context(&mut device, Duration::from_secs(1));
        assert_eq!(device.reads, MAX_READS);
        assert_eq!(context.bytes, MAX_READS * 4);
        assert!(context.issue.as_deref().unwrap().contains("32 reads"));
        let mut device = Device {
            fail: true,
            ..Device::default()
        };
        let context = map.prepare_chunk_context(&mut device, Duration::from_secs(1));
        assert_eq!(device.reads, 1);
        assert!(context.issue.as_deref().unwrap().contains("offline"));
        let mut device = Device {
            delay: Duration::from_millis(5),
            ..Device::default()
        };
        let context = map.prepare_chunk_context(&mut device, Duration::from_millis(1));
        assert_eq!(device.reads, 1);
        assert!(context.issue.as_deref().unwrap().contains("deadline"));
    }

    #[test]
    fn preparation_caps_retained_dependency_bytes_and_reuses_subranges() {
        let mut device = Device::default();
        let mut prepare = Prepare {
            device: &mut device,
            context: ChunkContext::default(),
            deadline: Instant::now() + Duration::from_secs(1),
        };
        assert_eq!(prepare.read_memory(10, MAX_BYTES).unwrap().len(), MAX_BYTES);
        assert_eq!(prepare.read_memory(12, 8).unwrap(), vec![1; 8]);
        assert_eq!(prepare.context.reads, 1);
        assert_eq!(prepare.context.bytes, MAX_BYTES);
        prepare
            .read_memory(10 + u64::try_from(MAX_BYTES).unwrap(), 1)
            .unwrap_err();
        assert_eq!(prepare.context.reads, 1);
        assert_eq!(prepare.context.bytes, MAX_BYTES);
    }

    #[test]
    fn interpretation_bounds_candidates_and_serialized_output() {
        let mut body = "<Port Name='Chunk'><ChunkID>00000012</ChunkID></Port><IntReg Name='Raw'><Address>0</Address><Length>4</Length><pPort>Chunk</pPort></IntReg>".to_owned();
        for index in 0..300 {
            write!(
                body,
                "<Integer Name='F{index:03}'><pValue>Raw</pValue></Integer>"
            )
            .unwrap();
        }
        let map = parse_map(&body);
        let values = map.chunk_values(&payload(&[(0x12, &[1; 4])]), &ChunkContext::default());
        assert_eq!(values["values"].as_object().unwrap().len(), MAX_CANDIDATES);
        assert_eq!(values["truncated"], true);
        assert!(serde_json::to_vec(&values).unwrap().len() <= MAX_OUTPUT);
        let map = parse_map(
            "<Port Name='Chunk'><ChunkID>00000012</ChunkID></Port><StringReg Name='Text'><Address>0</Address><Length>65536</Length><pPort>Chunk</pPort></StringReg>",
        );
        let bytes = vec![b'"'; 65536];
        let values = map.chunk_values(&payload(&[(0x12, &bytes)]), &ChunkContext::default());
        assert_eq!(values["truncated"], true);
        assert!(serde_json::to_vec(&values).unwrap().len() <= MAX_OUTPUT);
        let unit = "x".repeat(MAX_OUTPUT);
        let map = parse_map(&format!(
            "<Category Name='Root'><pFeature>Value</pFeature></Category><Integer Name='Value'><pValue>Raw</pValue><Unit>{unit}</Unit></Integer><IntReg Name='Raw'><Address>0</Address><Length>4</Length><pPort>Chunk</pPort></IntReg><Port Name='Chunk'><ChunkID>00000012</ChunkID></Port>"
        ));
        let values = map.chunk_values(&payload(&[(0x12, &[1; 4])]), &ChunkContext::default());
        assert_eq!(values["truncated"], true);
        assert!(serde_json::to_vec(&values).unwrap().len() <= MAX_OUTPUT);
    }
}
