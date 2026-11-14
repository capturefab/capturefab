//! One explicit write-only key target; generic feature transactions stay unchanged.
use super::{MAX_DEPTH, Node, NodeMap, encode_integer, exact_integer, integer_literal};
use crate::types::{Categorized, ErrorKind, RegisterIo};
use anyhow::{Context as _, Result};
use serde_json::json;

const FEATURE: &str = "ActionDeviceKey";

/// Static layout is known before any dependency reads, so the camera can forbid
/// every read overlapping this key, including aliases and aligned native reads.
#[derive(Debug)]
pub(crate) struct ActionDeviceKeyTarget {
    address: u64,
    length: usize,
    names: Vec<String>,
    big_endian: bool,
    signed: bool,
    port: Option<String>,
}
impl ActionDeviceKeyTarget {
    pub(crate) const fn address(&self) -> u64 {
        self.address
    }
    pub(crate) const fn length(&self) -> usize {
        self.length
    }
}

fn unsupported(message: &str) -> anyhow::Error {
    Categorized::new(ErrorKind::Unsupported, format!("{FEATURE}: {message}"))
        .details(json!({"feature":FEATURE}))
        .into()
}

fn single<'a>(node: &'a Node, key: &str) -> Result<Option<&'a str>> {
    let mut entries = node.props.iter().filter(|entry| entry.tag == key);
    let value = entries.next().map(|entry| entry.text.as_str());
    if entries.next().is_some() {
        return Err(unsupported("duplicate target properties are unsupported"));
    }
    Ok(value)
}

fn permissions(node: &Node, readable: &mut bool, writable: &mut bool) -> Result<()> {
    for key in ["AccessMode", "ImposedAccessMode"] {
        match single(node, key)? {
            None | Some("RW") => {}
            Some("RO") => *writable = false,
            Some("WO") => *readable = false,
            Some(_) => return Err(unsupported("unsupported or unavailable access mode")),
        }
    }
    Ok(())
}

fn gates(map: &NodeMap, node: &Node, depth: usize) -> Result<()> {
    for key in ["pIsImplemented", "pIsAvailable", "pIsLocked", "pError"] {
        if let Some(link) = single(node, key)? {
            map.node(link, depth + 1)?;
        }
    }
    Ok(())
}

pub(super) fn target(map: &NodeMap) -> Result<ActionDeviceKeyTarget> {
    let mut names = Vec::new();
    let mut name = FEATURE;
    let (mut readable, mut writable) = (true, true);
    loop {
        let node = map.node(name, names.len())?;
        if !matches!(node.kind.as_str(), "Integer" | "IntReg") {
            return Err(unsupported("requires a direct Integer-to-IntReg path"));
        }
        if names.len() >= MAX_DEPTH || names.iter().any(|old| old == name) {
            return Err(unsupported("cyclic or excessive target chain"));
        }
        permissions(node, &mut readable, &mut writable)?;
        if node.props.iter().any(|entry| {
            matches!(
                entry.tag.as_str(),
                "pValueCopy"
                    | "Value"
                    | "ValueIndexed"
                    | "pValueIndexed"
                    | "ValueDefault"
                    | "pValueDefault"
                    | "pIndex"
                    | "pAlias"
                    | "Alias"
            )
        }) {
            return Err(unsupported(
                "copies, host values and indexed value paths are unsupported",
            ));
        }
        for key in ["Min", "Max", "Inc"] {
            let literal = single(node, key)?;
            let linked = single(node, &format!("p{key}"))?;
            if literal.is_some() && linked.is_some() {
                return Err(unsupported("ambiguous numeric bounds"));
            }
        }
        gates(map, node, names.len())?;
        names.push(name.to_owned());
        if node.kind == "Integer" {
            name = single(node, "pValue")?
                .ok_or_else(|| unsupported("integer target has no direct pValue"))?;
            continue;
        }
        if single(node, "pValue")?.is_some()
            || node.props.iter().any(|entry| {
                matches!(
                    entry.tag.as_str(),
                    "pAddress" | "pLength" | "IntSwissKnife" | "Bit" | "LSB" | "MSB"
                )
            })
        {
            return Err(unsupported(
                "dynamic, linked or masked register layouts are unsupported",
            ));
        }
        let length = single(node, "Length")?
            .ok_or_else(|| unsupported("requires an explicit register Length"))?;
        let length = usize::try_from(exact_integer(&integer_literal(length)?)?)
            .context("ActionDeviceKey register length is out of range")?;
        if !matches!(length, 1 | 2 | 4 | 8) {
            return Err(unsupported(
                "requires a complete 1, 2, 4 or 8-byte integer register",
            ));
        }
        let big_endian = match single(node, "Endianess")? {
            Some("BigEndian") => true,
            Some("LittleEndian") => false,
            _ => return Err(unsupported("requires an explicit supported Endianess")),
        };
        let signed = match single(node, "Sign")? {
            None | Some("Unsigned") => false,
            Some("Signed") => true,
            Some(_) => return Err(unsupported("unsupported integer Sign")),
        };
        let mut address = 0_i128;
        let mut addresses = 0;
        for entry in node.props.iter().filter(|entry| entry.tag == "Address") {
            address = address
                .checked_add(exact_integer(&integer_literal(&entry.text)?)?)
                .context("ActionDeviceKey address sum overflow")?;
            addresses += 1;
        }
        if addresses == 0 {
            return Err(unsupported("requires an explicit static register Address"));
        }
        let address = u64::try_from(address).context("ActionDeviceKey address is out of range")?;
        address
            .checked_add(u64::try_from(length)?)
            .context("ActionDeviceKey register range overflow")?;
        let port = single(node, "pPort")?;
        if let Some(name) = port {
            let port = map.node(name, names.len())?;
            if port.kind != "Port"
                || port.props.iter().any(|entry| {
                    matches!(
                        entry.tag.as_str(),
                        "ChunkID" | "pChunkID" | "EventID" | "pEventID" | "pPort"
                    )
                })
                || single(port, "PortName")?.is_some_and(|name| name != "Device")
            {
                return Err(unsupported("requires a live device register port"));
            }
            permissions(port, &mut readable, &mut writable)?;
            gates(map, port, names.len())?;
        }
        if !writable {
            return Err(Categorized::new(
                ErrorKind::NotWritable,
                format!("{FEATURE} is not writable"),
            )
            .details(json!({"feature":FEATURE}))
            .into());
        }
        if readable {
            return Err(unsupported(
                "is readable; use the ordinary transactional setter",
            ));
        }
        return Ok(ActionDeviceKeyTarget {
            address,
            length,
            names,
            big_endian,
            signed,
            port: port.map(str::to_owned),
        });
    }
}

fn bound(
    map: &NodeMap,
    io: &mut dyn RegisterIo,
    node: &Node,
    key: &str,
    depth: usize,
) -> Result<Option<i128>> {
    single(node, &format!("p{key}"))?.map_or_else(
        || {
            single(node, key)?
                .map(|text| exact_integer(&integer_literal(text)?))
                .transpose()
        },
        |link| exact_integer(&map.numeric_value(io, link, depth + 1)?).map(Some),
    )
}

fn out_of_range() -> anyhow::Error {
    // The key is intentionally absent from diagnostics, history and results.
    Categorized::new(
        ErrorKind::OutOfRange,
        "ActionDeviceKey violates declared integer bounds",
    )
    .details(json!({"feature":FEATURE}))
    .into()
}

pub(super) fn write(
    map: &NodeMap,
    io: &mut dyn RegisterIo,
    target: &ActionDeviceKeyTarget,
    key: u32,
) -> Result<()> {
    let bits = target.length * 8;
    let max = (1_i128 << (bits - usize::from(target.signed))) - 1;
    if i128::from(key) > max {
        return Err(out_of_range());
    }
    let mut bounds = Vec::with_capacity(target.names.len());
    if let Some(port) = &target.port {
        map.access(io, port, true, target.names.len())?;
    }
    for (depth, name) in target.names.iter().enumerate() {
        map.access(io, name, true, depth)?;
        let node = map.node(name, depth)?;
        bounds.push((
            bound(map, io, node, "Min", depth)?,
            bound(map, io, node, "Max", depth)?,
            bound(map, io, node, "Inc", depth)?,
        ));
    }
    for (index, (min, max, inc)) in bounds.iter().copied().enumerate() {
        if min.zip(max).is_some_and(|(min, max)| min > max) || inc.is_some_and(|inc| inc <= 0) {
            return Err(unsupported("invalid camera integer bounds"));
        }
        let value = i128::from(key);
        if min.is_some_and(|min| value < min) || max.is_some_and(|max| value > max) {
            return Err(out_of_range());
        }
        if let Some(inc) = inc {
            let minimum = if min.is_none() && index + 1 < bounds.len() {
                bounds
                    .iter()
                    .skip(index + 1)
                    .find_map(|(min, _, _)| *min)
                    .unwrap_or_else(|| {
                        if target.signed {
                            -(1_i128 << (bits - 1))
                        } else {
                            0
                        }
                    })
            } else {
                min.unwrap_or(0)
            };
            if (value - minimum) % inc != 0 {
                return Err(out_of_range());
            }
        }
    }
    let bytes = encode_integer(u64::from(key), target.length, target.big_endian);
    io.write_memory(target.address, &bytes)?;
    if let Some(port) = &target.port {
        map.check_error(io, map.node(port, target.names.len())?, target.names.len())?;
    }
    for (depth, name) in target.names.iter().enumerate().rev() {
        map.check_error(io, map.node(name, depth)?, depth)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::classify;
    use std::collections::BTreeMap;

    #[derive(Default)]
    struct Device {
        words: BTreeMap<u64, u32>,
        reads: Vec<(u64, usize)>,
        writes: Vec<(u64, Vec<u8>)>,
    }
    impl RegisterIo for Device {
        fn read_memory(&mut self, address: u64, length: usize) -> Result<Vec<u8>> {
            self.reads.push((address, length));
            let word = self.words.get(&address).copied().unwrap_or(0).to_le_bytes();
            if length != word.len() {
                return Err(anyhow::anyhow!("fixture read length"));
            }
            Ok(word.to_vec())
        }
        fn write_memory(&mut self, address: u64, data: &[u8]) -> Result<()> {
            self.writes.push((address, data.to_vec()));
            Ok(())
        }
    }
    fn map(body: &str) -> NodeMap {
        NodeMap::parse(&format!(
            "<RegisterDescription>{body}</RegisterDescription>"
        ))
        .unwrap()
    }
    fn key(wrapper: &str, register: &str) -> NodeMap {
        map(&format!(
            "<Integer Name='ActionDeviceKey'>{wrapper}<pValue>Key</pValue></Integer><IntReg Name='Key'><Address>0x1000</Address><Length>4</Length>{register}</IntReg>"
        ))
    }
    const LITTLE: &str = "<Endianess>LittleEndian</Endianess>";

    #[test]
    fn effective_wo_intersection_writes_one_declared_integer_without_key_reads() {
        for (upper, lower) in [
            ("<AccessMode>WO</AccessMode>", "<AccessMode>RW</AccessMode>"),
            ("<AccessMode>RW</AccessMode>", "<AccessMode>WO</AccessMode>"),
            (
                "<ImposedAccessMode>WO</ImposedAccessMode>",
                "<AccessMode>RW</AccessMode>",
            ),
        ] {
            for (endian, bytes) in [
                ("LittleEndian", [0x78, 0x56, 0x34, 0x12]),
                ("BigEndian", [0x12, 0x34, 0x56, 0x78]),
            ] {
                let map = key(upper, &format!("{lower}<Endianess>{endian}</Endianess>"));
                let target = map.action_device_key_target().unwrap();
                assert_eq!((target.address, target.length), (0x1000, 4));
                let mut io = Device::default();
                map.write_action_device_key(&mut io, &target, 0x1234_5678)
                    .unwrap();
                assert_eq!(io.reads, [] as [(u64, usize); 0]);
                assert_eq!(io.writes, [(0x1000, bytes.to_vec())]);
            }
        }
        let direct = map(
            "<IntReg Name='ActionDeviceKey'><Address>0x11000</Address><Length>8</Length><Endianess>BigEndian</Endianess><AccessMode>WO</AccessMode></IntReg>",
        );
        let mut io = Device::default();
        direct
            .write_action_device_key(
                &mut io,
                &direct.action_device_key_target().unwrap(),
                u32::MAX,
            )
            .unwrap();
        assert_eq!(
            io.writes,
            [(0x0001_1000, vec![0, 0, 0, 0, 0xff, 0xff, 0xff, 0xff])]
        );
        assert_eq!(io.reads, [] as [(u64, usize); 0]);
        let port_map = map(
            "<Integer Name='ActionDeviceKey'><pValue>Key</pValue></Integer><IntReg Name='Key'><Address>0x1000</Address><Length>4</Length><Endianess>LittleEndian</Endianess><pPort>Device</pPort></IntReg><Port Name='Device'><AccessMode>WO</AccessMode></Port>",
        );
        let mut io = Device::default();
        port_map
            .write_action_device_key(&mut io, &port_map.action_device_key_target().unwrap(), 0)
            .unwrap();
        assert_eq!(io.writes, [(0x1000, vec![0; 4])]);
    }

    #[test]
    fn readable_or_readonly_and_unsupported_graphs_fail_during_static_preflight() {
        for (upper, lower) in [
            ("<AccessMode>RW</AccessMode>", "<AccessMode>RW</AccessMode>"),
            ("<AccessMode>WO</AccessMode>", "<AccessMode>RO</AccessMode>"),
            ("<AccessMode>RO</AccessMode>", "<AccessMode>WO</AccessMode>"),
            ("<AccessMode>NA</AccessMode>", "<AccessMode>WO</AccessMode>"),
            ("<Value>1</Value>", "<AccessMode>WO</AccessMode>"),
            (
                "<pValueCopy>Key</pValueCopy>",
                "<AccessMode>WO</AccessMode>",
            ),
            (
                "<pValueIndexed Index='0'>Key</pValueIndexed>",
                "<AccessMode>WO</AccessMode>",
            ),
        ] {
            let map = key(upper, &format!("{lower}{LITTLE}"));
            drop(map.action_device_key_target().unwrap_err());
        }
        for kind in [
            "IntConverter",
            "MaskedIntReg",
            "FloatReg",
            "Register",
            "Boolean",
        ] {
            let map = map(&format!(
                "<{kind} Name='ActionDeviceKey'><AccessMode>WO</AccessMode><Address>0x1000</Address><Length>4</Length>{LITTLE}</{kind}>"
            ));
            assert_eq!(
                classify(&map.action_device_key_target().err().unwrap()).kind,
                ErrorKind::Unsupported
            );
        }
        let map = map(
            "<Integer Name='ActionDeviceKey'><AccessMode>WO</AccessMode><pValue>Again</pValue></Integer><Integer Name='Again'><pValue>ActionDeviceKey</pValue></Integer>",
        );
        drop(map.action_device_key_target().unwrap_err());
    }

    #[test]
    fn static_layout_requires_explicit_endian_complete_length_and_safe_live_port() {
        for body in [
            "<Address>0x1000</Address><Length>3</Length><Endianess>LittleEndian</Endianess>",
            "<Address>0x1000</Address><Length>4</Length>",
            "<Address>0x1000</Address><Length>4</Length><Endianess>Unknown</Endianess>",
            "<Address>-1</Address><Length>4</Length><Endianess>LittleEndian</Endianess>",
            "<Address>0xffffffffffffffff</Address><Length>4</Length><Endianess>LittleEndian</Endianess>",
            "<Address>0x1000</Address><Length>4</Length><pLength>Length</pLength><Endianess>LittleEndian</Endianess>",
            "<Address>0x1000</Address><pAddress>Address</pAddress><Length>4</Length><Endianess>LittleEndian</Endianess>",
            "<Address>0x1000</Address><pIndex>Index</pIndex><Length>4</Length><Endianess>LittleEndian</Endianess>",
        ] {
            let map = map(&format!(
                "<IntReg Name='ActionDeviceKey'><AccessMode>WO</AccessMode>{body}</IntReg>"
            ));
            drop(map.action_device_key_target().unwrap_err());
        }
        let chunk_map = map(&format!(
            "<IntReg Name='ActionDeviceKey'><AccessMode>WO</AccessMode><Address>0x1000</Address><Length>4</Length>{LITTLE}<pPort>Chunk</pPort></IntReg><Port Name='Chunk'><ChunkID>1</ChunkID></Port>"
        ));
        drop(chunk_map.action_device_key_target().unwrap_err());
        for properties in ["<PortName>System</PortName>", "<AccessMode>RO</AccessMode>"] {
            let port_map = map(&format!(
                "<IntReg Name='ActionDeviceKey'><AccessMode>WO</AccessMode><Address>0x1000</Address><Length>4</Length>{LITTLE}<pPort>Device</pPort></IntReg><Port Name='Device'>{properties}</Port>"
            ));
            drop(port_map.action_device_key_target().unwrap_err());
        }
    }

    #[test]
    fn full_chain_bounds_gates_and_postwrite_error_use_only_other_registers() {
        let map = map(&format!(
            "<Integer Name='ActionDeviceKey'><AccessMode>WO</AccessMode><pValue>Alias</pValue><pIsAvailable>Available</pIsAvailable><pIsLocked>Locked</pIsLocked><pMin>Minimum</pMin><Max>100</Max><pError>Status</pError></Integer><Integer Name='Alias'><pValue>Key</pValue><Min>10</Min><Inc>2</Inc></Integer><IntReg Name='Key'><Address>0x1000</Address><Length>4</Length>{LITTLE}<pPort>Device</pPort></IntReg><Port Name='Device'><pIsAvailable>Available</pIsAvailable><pError>Status</pError></Port><IntReg Name='Available'><Address>0x20</Address><Length>4</Length></IntReg><IntReg Name='Locked'><Address>0x24</Address><Length>4</Length></IntReg><IntReg Name='Minimum'><Address>0x28</Address><Length>4</Length></IntReg><IntReg Name='Status'><Address>0x30</Address><Length>4</Length></IntReg>"
        ));
        let target = map.action_device_key_target().unwrap();
        let mut io = Device::default();
        io.words.extend([(0x20, 1), (0x28, 10)]);
        map.write_action_device_key(&mut io, &target, 40).unwrap();
        assert_eq!(io.writes, [(0x1000, vec![40, 0, 0, 0])]);
        assert!(io.reads.iter().all(|(address, _)| *address != 0x1000));
        assert_eq!(io.reads.last(), Some(&(0x30, 4)));
        for (address, value, kind) in [
            (0x20, 0, ErrorKind::OperationFailed),
            (0x24, 1, ErrorKind::FeatureLocked),
            (0x28, 41, ErrorKind::OutOfRange),
        ] {
            let mut io = Device::default();
            io.words.extend([(0x20, 1), (0x28, 10), (address, value)]);
            let error = map
                .write_action_device_key(&mut io, &target, 40)
                .unwrap_err();
            assert_eq!(classify(&error).kind, kind);
            assert_eq!(io.writes, [] as [(u64, Vec<u8>); 0]);
        }
        io.words.insert(0x30, 7);
        assert_eq!(
            classify(
                &map.write_action_device_key(&mut io, &target, 40)
                    .unwrap_err()
            )
            .kind,
            ErrorKind::OperationFailed
        );
        assert_eq!(
            io.writes.len(),
            2,
            "postwrite device error does not fabricate rollback"
        );
    }

    #[test]
    fn register_width_sign_and_inherited_increment_bounds_reject_before_mutation() {
        for (length, sign, value) in [
            (1, "Unsigned", 256),
            (2, "Unsigned", 65_536),
            (4, "Signed", u32::MAX),
        ] {
            let map = map(&format!(
                "<IntReg Name='ActionDeviceKey'><AccessMode>WO</AccessMode><Address>0x1000</Address><Length>{length}</Length>{LITTLE}<Sign>{sign}</Sign></IntReg>"
            ));
            let mut io = Device::default();
            let error = map
                .write_action_device_key(&mut io, &map.action_device_key_target().unwrap(), value)
                .unwrap_err();
            assert_eq!(classify(&error).kind, ErrorKind::OutOfRange);
            assert!(classify(&error).details.unwrap().get("value").is_none());
            assert_eq!(io.writes, [] as [(u64, Vec<u8>); 0]);
        }
        let map = key(
            "<AccessMode>WO</AccessMode><Inc>4</Inc>",
            &format!("{LITTLE}<Min>1</Min>"),
        );
        let target = map.action_device_key_target().unwrap();
        let mut io = Device::default();
        map.write_action_device_key(&mut io, &target, 5).unwrap();
        assert_eq!(
            classify(
                &map.write_action_device_key(&mut io, &target, 6)
                    .unwrap_err()
            )
            .kind,
            ErrorKind::OutOfRange
        );
        assert_eq!(io.writes.len(), 1);
    }
}
