//! A small, uncached GenApi interpreter. Unsupported nodes and expressions fail closed.
//! Every feature operation resolves its links again, so selector-dependent addresses and
//! access conditions are evaluated against the camera's current state.
use crate::types::RegisterIo;
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    io::{Cursor, Read},
};

const MAX_XML: usize = 32 * 1024 * 1024;
const MAX_DEPTH: usize = 64;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeatureInfo {
    pub name: String,
    pub display_name: String,
    pub kind: String,
    pub value: Option<Value>,
    pub writable: bool,
    pub description: String,
    pub unit: Option<String>,
    pub min: Option<f64>,
    pub max: Option<f64>,
    #[serde(default)]
    pub inc: Option<f64>,
    pub choices: Vec<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Bounds {
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub inc: Option<f64>,
}

#[derive(Debug, Clone)]
struct Element {
    tag: String,
    text: String,
    attrs: BTreeMap<String, String>,
}
#[derive(Debug, Clone)]
struct Node {
    kind: String,
    props: Vec<Element>,
    stored: RefCell<Option<Value>>,
}
impl Node {
    fn prop(&self, key: &str) -> Option<&str> {
        self.props
            .iter()
            .find(|p| p.tag == key)
            .map(|p| p.text.as_str())
    }
    fn big_endian(&self) -> bool {
        self.prop("Endianess").unwrap_or("LittleEndian") == "BigEndian"
    }
    fn host_value(&self) -> bool {
        matches!(
            self.kind.as_str(),
            "Integer" | "Float" | "Boolean" | "Enumeration"
        ) && self.prop("Value").is_some()
    }
    fn value(&self, text: &str) -> Result<Value> {
        self.stored
            .borrow()
            .clone()
            .map_or_else(|| literal(text), Ok)
    }
}

#[derive(Debug, Clone)]
pub struct NodeMap {
    nodes: BTreeMap<String, Node>,
}

impl NodeMap {
    pub fn parse(xml: &str) -> Result<Self> {
        ensure!(xml.len() <= MAX_XML, "GenICam XML exceeds 32 MiB limit");
        let doc =
            roxmltree::Document::parse(xml.trim_matches('\0')).context("invalid GenICam XML")?;
        ensure!(
            doc.root_element().tag_name().name() == "RegisterDescription",
            "expected GenICam RegisterDescription root"
        );
        let mut nodes = BTreeMap::new();
        let props = |n: roxmltree::Node<'_, '_>| {
            n.children()
                .filter(|p| p.is_element())
                .map(|p| Element {
                    tag: p.tag_name().name().into(),
                    text: p.text().unwrap_or("").trim().into(),
                    attrs: p
                        .attributes()
                        .map(|a| (a.name().into(), a.value().into()))
                        .collect(),
                })
                .collect::<Vec<_>>()
        };
        for child in doc.root_element().descendants().filter(|n| {
            n.is_element()
                && n.parent()
                    .is_some_and(|p| matches!(p.tag_name().name(), "RegisterDescription" | "Group"))
        }) {
            let parent_name = child.attribute("Name");
            if let Some(name) = parent_name {
                let mut fields = props(child);
                for field in fields.iter_mut().filter(|p| p.tag == "EnumEntry") {
                    if let Some(entry_name) = field.attrs.get("Name").cloned() {
                        field
                            .attrs
                            .insert("Name".into(), format!("__enum::{name}::{entry_name}"));
                    }
                }
                ensure!(!nodes.contains_key(name), "duplicate GenICam node {name}");
                nodes.insert(
                    name.into(),
                    Node {
                        kind: child.tag_name().name().into(),
                        props: fields,
                        stored: RefCell::default(),
                    },
                );
            }
            // Nested EnumEntry names are scoped to their owning enumeration.
            // StructReg itself may be unnamed; its entries are named features.
            for entry in child.children().filter(|n| {
                n.is_element() && matches!(n.tag_name().name(), "EnumEntry" | "StructEntry")
            }) {
                let Some(entry_name) = entry.attribute("Name") else {
                    continue;
                };
                let mut fields = props(entry);
                let (kind, stored_name) = if entry.tag_name().name() == "StructEntry" {
                    let inherited = props(child)
                        .into_iter()
                        .filter(|p| !fields_has(&fields, &p.tag) && p.tag != "StructEntry")
                        .collect::<Vec<_>>();
                    fields.extend(inherited);
                    ("MaskedIntReg", entry_name.to_string())
                } else {
                    let parent = parent_name.context("nested EnumEntry has no named parent")?;
                    if !fields_has(&fields, "Symbolic") {
                        fields.push(Element {
                            tag: "Symbolic".into(),
                            text: entry_name.into(),
                            attrs: BTreeMap::new(),
                        });
                    }
                    ("EnumEntry", format!("__enum::{parent}::{entry_name}"))
                };
                ensure!(
                    !nodes.contains_key(&stored_name),
                    "duplicate GenICam node {stored_name}"
                );
                nodes.insert(
                    stored_name,
                    Node {
                        kind: kind.into(),
                        props: fields,
                        stored: RefCell::default(),
                    },
                );
            }
        }
        ensure!(!nodes.is_empty(), "GenICam XML has no named nodes");
        Ok(Self { nodes })
    }
    pub fn has(&self, name: &str) -> bool {
        self.nodes.contains_key(name)
    }
    pub fn is_writable(&self, io: &mut dyn RegisterIo, name: &str) -> bool {
        self.writable(io, name, 0).is_ok()
    }
    pub fn choices(&self, io: &mut dyn RegisterIo, name: &str) -> Result<Vec<String>> {
        let node = self.node(name, 0)?;
        Ok(self.entry_names(io, name, node))
    }
    fn entry_names(&self, io: &mut dyn RegisterIo, name: &str, node: &Node) -> Vec<String> {
        self.enum_entries(node)
            .into_iter()
            .filter_map(|e| {
                self.nodes
                    .get(e)
                    .filter(|n| self.available(io, n, 0).is_ok())
                    .map(|en| {
                        en.prop("Symbolic")
                            .unwrap_or_else(|| e.strip_prefix(&format!("{name}_")).unwrap_or(e))
                            .to_string()
                    })
            })
            .collect()
    }
    pub fn bounds(&self, io: &mut dyn RegisterIo, name: &str) -> Result<Bounds> {
        let node = self.node(name, 0)?;
        Ok(self.range(io, node, 0))
    }
    fn range(&self, io: &mut dyn RegisterIo, n: &Node, depth: usize) -> Bounds {
        let delegated = matches!(
            n.kind.as_str(),
            "Integer" | "Float" | "Converter" | "IntConverter"
        ) && ["Min", "Max", "Inc", "pMin", "pMax", "pInc"]
            .iter()
            .all(|key| n.prop(key).is_none());
        if !delegated {
            let mut own = |key| self.property_number(io, n, key, depth).ok().flatten();
            return Bounds {
                min: own("Min"),
                max: own("Max"),
                inc: own("Inc"),
            };
        }
        let inner = self
            .resolve_value_link(io, n, depth)
            .ok()
            .flatten()
            .and_then(|link| self.node(link, depth + 1).ok())
            .map(|inner| self.range(io, inner, depth + 1))
            .unwrap_or_default();
        if matches!(n.kind.as_str(), "Integer" | "Float") {
            return inner;
        }
        let mut from = |v: Option<f64>| {
            v.and_then(|v| {
                self.formula(io, n, "FormulaFrom", Some(("TO", v)), depth)
                    .ok()
            })
        };
        match (from(inner.min), from(inner.max)) {
            (Some(a), Some(b)) => Bounds {
                min: Some(a.min(b)),
                max: Some(a.max(b)),
                inc: None,
            },
            _ => Bounds::default(),
        }
    }
    fn node(&self, name: &str, depth: usize) -> Result<&Node> {
        ensure!(
            depth <= MAX_DEPTH,
            "GenICam dependency cycle or excessive nesting at {name}"
        );
        self.nodes
            .get(name)
            .ok_or_else(|| anyhow!("unknown GenICam feature {name}"))
    }
    fn available(&self, io: &mut dyn RegisterIo, node: &Node, depth: usize) -> Result<()> {
        for key in ["pIsImplemented", "pIsAvailable"] {
            if let Some(link) = node.prop(key) {
                ensure!(
                    self.number(io, link, depth + 1)? != 0.0,
                    "feature is {}",
                    if key == "pIsImplemented" {
                        "not implemented"
                    } else {
                        "unavailable"
                    }
                );
            }
        }
        Ok(())
    }
    fn access(&self, io: &mut dyn RegisterIo, name: &str, write: bool, depth: usize) -> Result<()> {
        let node = self.node(name, depth)?;
        self.available(io, node, depth)?;
        for key in ["AccessMode", "ImposedAccessMode"] {
            if let Some(mode) = node.prop(key) {
                ensure!(
                    matches!(mode, "RO" | "WO" | "RW"),
                    "{name} has unsupported or unavailable access mode {mode}"
                );
                ensure!(
                    if write { mode != "RO" } else { mode != "WO" },
                    "{name} is {}",
                    if write { "read-only" } else { "write-only" }
                );
            }
        }
        if write && let Some(link) = node.prop("pIsLocked") {
            ensure!(self.number(io, link, depth + 1)? == 0.0, "{name} is locked");
        }
        Ok(())
    }
    pub fn features(&self, io: &mut dyn RegisterIo) -> Vec<FeatureInfo> {
        let exposed = self.exposed_features();
        self.nodes
            .iter()
            .filter(|(name, n)| {
                (exposed.as_ref().is_none_or(|names| names.contains(*name)))
                    && matches!(
                        n.kind.as_str(),
                        "Integer"
                            | "Float"
                            | "Boolean"
                            | "Enumeration"
                            | "String"
                            | "Command"
                            | "IntReg"
                            | "MaskedIntReg"
                            | "FloatReg"
                            | "StringReg"
                            | "IntSwissKnife"
                            | "SwissKnife"
                            | "IntConverter"
                            | "Converter"
                    )
                    && n.prop("Visibility") != Some("Invisible")
            })
            .map(|(name, node)| {
                let result = self.get(io, name);
                let (value, error) = match result {
                    Ok(v) => (Some(v), None),
                    Err(e) => (None, Some(format!("{e:#}"))),
                };
                let writable = self.writable(io, name, 0).is_ok();
                let choices = self.entry_names(io, name, node);
                let bounds = self.bounds(io, name).unwrap_or_default();
                FeatureInfo {
                    name: name.clone(),
                    display_name: node.prop("DisplayName").unwrap_or(name).into(),
                    kind: node.kind.clone(),
                    value,
                    writable,
                    description: node
                        .prop("Description")
                        .or_else(|| node.prop("ToolTip"))
                        .unwrap_or("")
                        .into(),
                    unit: node.prop("Unit").map(str::to_string),
                    min: bounds.min,
                    max: bounds.max,
                    inc: bounds.inc,
                    choices,
                    error,
                }
            })
            .collect()
    }
    fn exposed_features(&self) -> Option<BTreeSet<String>> {
        if !self
            .nodes
            .get("Root")
            .is_some_and(|node| node.kind == "Category")
        {
            return None;
        }
        let mut features = BTreeSet::new();
        let mut pending = vec!["Root".to_string()];
        while let Some(name) = pending.pop() {
            if !features.insert(name.clone()) {
                continue;
            }
            if let Some(node) = self.nodes.get(&name)
                && node.kind == "Category"
            {
                pending.extend(
                    node.props
                        .iter()
                        .filter(|p| p.tag == "pFeature")
                        .map(|p| p.text.clone()),
                );
            }
        }
        Some(features)
    }
    fn writable(&self, io: &mut dyn RegisterIo, name: &str, depth: usize) -> Result<()> {
        self.access(io, name, true, depth)?;
        let n = self.node(name, depth)?;
        ensure!(
            matches!(
                n.kind.as_str(),
                "Integer"
                    | "Float"
                    | "String"
                    | "Boolean"
                    | "Enumeration"
                    | "Command"
                    | "Converter"
                    | "IntConverter"
                    | "IntReg"
                    | "MaskedIntReg"
                    | "FloatReg"
                    | "StringReg"
            ),
            "unsupported writable node type {}",
            n.kind
        );
        if let Some(link) = self.resolve_value_link(io, n, depth)? {
            return self.writable(io, link, depth + 1);
        }
        ensure!(
            n.host_value()
                || matches!(
                    n.kind.as_str(),
                    "IntReg" | "MaskedIntReg" | "FloatReg" | "StringReg"
                ),
            "{name} has no writable register"
        );
        Ok(())
    }
    pub fn get(&self, io: &mut dyn RegisterIo, name: &str) -> Result<Value> {
        self.get_inner(io, name, 0)
    }
    fn get_inner(&self, io: &mut dyn RegisterIo, name: &str, depth: usize) -> Result<Value> {
        self.access(io, name, false, depth)?;
        let n = self.node(name, depth)?;
        match n.kind.as_str() {
            "IntReg" | "MaskedIntReg" => {
                let (addr, len) = self.register(io, n, depth)?;
                ensure!(
                    matches!(len, 1 | 2 | 4 | 8),
                    "integer register length {len} is unsupported"
                );
                let bytes = io.read_memory(addr, len)?;
                ensure!(bytes.len() == len, "short register read");
                let mut raw = decode_integer(&bytes, n.big_endian());
                let bits = if n.kind == "MaskedIntReg" {
                    let (shift, bits) = self.mask(n, len)?;
                    raw = (raw >> shift) & bit_mask(bits);
                    bits
                } else {
                    len * 8
                };
                if n.prop("Sign") == Some("Signed") {
                    let signed = if bits == 64 {
                        raw as i64
                    } else {
                        ((raw << (64 - bits)) as i64) >> (64 - bits)
                    };
                    Ok(json!(signed))
                } else {
                    Ok(json!(raw))
                }
            }
            "FloatReg" => {
                let (addr, len) = self.register(io, n, depth)?;
                let b = io.read_memory(addr, len)?;
                ensure!(b.len() == len, "short float register read");
                let v = match len {
                    4 => f32::from_bits(decode_integer(&b, n.big_endian()) as u32) as f64,
                    8 => f64::from_bits(decode_integer(&b, n.big_endian())),
                    _ => bail!("float register length {len} is unsupported"),
                };
                ensure!(
                    v.is_finite(),
                    "register contains non-finite floating point value"
                );
                Ok(json!(v))
            }
            "StringReg" => {
                let (addr, len) = self.register(io, n, depth)?;
                let b = io.read_memory(addr, len)?;
                ensure!(b.len() == len, "short string register read");
                Ok(json!(
                    std::str::from_utf8(b.split(|b| *b == 0).next().unwrap_or(&[]))
                        .context("non-UTF8 string register")?
                ))
            }
            "Integer" | "Float" | "String" | "EnumEntry" => {
                if let Some(link) = self.resolve_value_link(io, n, depth)? {
                    self.get_inner(io, link, depth + 1)
                } else if let Some(value) = n.prop("Value") {
                    if n.kind == "String" {
                        Ok(json!(value))
                    } else {
                        n.value(value)
                    }
                } else {
                    bail!("{name} has no supported pValue or Value")
                }
            }
            "Boolean" => {
                let value = self.value_number(io, n, depth)?;
                let on = self
                    .property_number(io, n, "OnValue", depth)?
                    .unwrap_or(1.0);
                let off = self
                    .property_number(io, n, "OffValue", depth)?
                    .unwrap_or(0.0);
                ensure!(
                    value == on || value == off,
                    "{name} contains an undefined boolean value"
                );
                Ok(json!(value == on))
            }
            "Enumeration" => {
                let value = self.value_number(io, n, depth)?;
                for entry in self.enum_entries(n) {
                    let en = self.node(entry, depth + 1)?;
                    if self.available(io, en, depth + 1).is_ok()
                        && self.value_number(io, en, depth + 1)? == value
                    {
                        return Ok(json!(en.prop("Symbolic").unwrap_or_else(|| {
                            entry.strip_prefix(&format!("{name}_")).unwrap_or(entry)
                        })));
                    }
                }
                bail!("{name} contains unknown enumeration value {value}")
            }
            "IntSwissKnife" | "SwissKnife" => {
                let value = self.formula(io, n, "Formula", None, depth)?;
                if n.kind == "IntSwissKnife" {
                    integer_formula(value)
                } else {
                    Ok(json!(value))
                }
            }
            "IntConverter" | "Converter" => {
                let input = self.value_number(io, n, depth)?;
                let value = self.formula(io, n, "FormulaFrom", Some(("TO", input)), depth)?;
                if n.kind == "IntConverter" {
                    integer_formula(value)
                } else {
                    Ok(json!(value))
                }
            }
            "Command" => Ok(Value::Null),
            other => bail!("unsupported GenApi node type {other} ({name})"),
        }
    }
    fn value_number(&self, io: &mut dyn RegisterIo, n: &Node, depth: usize) -> Result<f64> {
        if let Some(link) = self.resolve_value_link(io, n, depth)? {
            self.number(io, link, depth + 1)
        } else if let Some(v) = n.prop("Value") {
            number(&n.value(v)?)
        } else {
            bail!("node has no numeric value")
        }
    }
    fn number(&self, io: &mut dyn RegisterIo, name: &str, depth: usize) -> Result<f64> {
        let n = self.node(name, depth)?;
        if n.kind == "Enumeration" {
            self.access(io, name, false, depth)?;
            self.value_number(io, n, depth)
        } else {
            number(&self.get_inner(io, name, depth)?)
        }
    }
    /// Integer value multiplexers are distinct from register address indexing.
    /// Resolve the current selector on every operation rather than caching a
    /// branch that could be invalidated by another feature write.
    fn resolve_value_link<'a>(
        &self,
        io: &mut dyn RegisterIo,
        n: &'a Node,
        depth: usize,
    ) -> Result<Option<&'a str>> {
        if !n.props.iter().any(|p| p.tag == "pValueIndexed") {
            return Ok(n.prop("pValue").or_else(|| n.prop("pValueDefault")));
        }
        let selector = n
            .prop("pIndex")
            .context("indexed value node has no pIndex")?;
        let index = self.number(io, selector, depth + 1)?;
        ensure!(
            index.fract() == 0.0 && index.abs() <= (1u64 << 53) as f64,
            "indexed value selector is not an exact integer"
        );
        let mut selected = None;
        for entry in n.props.iter().filter(|p| p.tag == "pValueIndexed") {
            let candidate = number(&literal(
                entry
                    .attrs
                    .get("Index")
                    .context("pValueIndexed has no Index")?,
            )?)?;
            ensure!(
                candidate.fract() == 0.0 && candidate.abs() <= (1u64 << 53) as f64,
                "pValueIndexed Index is not an exact integer"
            );
            if candidate == index {
                ensure!(
                    selected.is_none(),
                    "duplicate pValueIndexed branch for index {index}"
                );
                selected = Some(entry.text.as_str());
            }
        }
        Ok(Some(
            selected
                .or_else(|| n.prop("pValueDefault"))
                .ok_or_else(|| {
                    anyhow!("indexed value has no branch or default for index {index}")
                })?,
        ))
    }
    fn property_number(
        &self,
        io: &mut dyn RegisterIo,
        n: &Node,
        key: &str,
        depth: usize,
    ) -> Result<Option<f64>> {
        if let Some(link) = n.prop(&format!("p{key}")) {
            Ok(Some(self.number(io, link, depth + 1)?))
        } else {
            n.prop(key).map(|s| number(&literal(s)?)).transpose()
        }
    }
    fn register(&self, io: &mut dyn RegisterIo, n: &Node, depth: usize) -> Result<(u64, usize)> {
        if let Some(port) = n.prop("pPort") {
            let port = self.node(port, depth + 1)?;
            ensure!(
                port.kind == "Port"
                    && !port.props.iter().any(|p| matches!(
                        p.tag.as_str(),
                        "ChunkID" | "pChunkID" | "EventID" | "pEventID"
                    )),
                "chunk and event ports are not supported for live register access"
            );
        }
        let mut address = 0u64;
        let mut supplied = false;
        for p in &n.props {
            let offset = match p.tag.as_str() {
                "Address" => {
                    supplied = true;
                    unsigned(&literal(&p.text)?)?
                }
                "pAddress" => {
                    supplied = true;
                    unsigned(&self.get_inner(io, &p.text, depth + 1)?)?
                }
                "pIndex" => {
                    let index = unsigned(&self.get_inner(io, &p.text, depth + 1)?)?;
                    let stride = unsigned(&literal(
                        p.attrs.get("Offset").map(String::as_str).unwrap_or("1"),
                    )?)?;
                    index
                        .checked_mul(stride)
                        .context("register index overflow")?
                }
                _ => continue,
            };
            address = address
                .checked_add(offset)
                .context("register address overflow")?;
        }
        ensure!(supplied, "register has no address");
        let length = if let Some(p) = n.prop("pLength") {
            unsigned(&self.get_inner(io, p, depth + 1)?)?
        } else {
            unsigned(&literal(
                n.prop("Length").context("register has no length")?,
            )?)?
        };
        ensure!(
            length > 0 && length <= 1024 * 1024,
            "register length is outside 1..1048576"
        );
        address
            .checked_add(length)
            .context("register range overflow")?;
        Ok((address, length as usize))
    }
    fn mask(&self, n: &Node, len: usize) -> Result<(usize, usize)> {
        let parse = |key| -> Result<usize> {
            Ok(unsigned(&literal(
                n.prop(key)
                    .ok_or_else(|| anyhow!("masked register missing {key}"))?,
            )?)? as usize)
        };
        let (lsb, msb) = if n.prop("Bit").is_some() {
            let bit = parse("Bit")?;
            (bit, bit)
        } else {
            (parse("LSB")?, parse("MSB")?)
        };
        ensure!(lsb < len * 8 && msb < len * 8, "register mask out of range");
        // GenApi numbers big-endian register bits from the most significant bit.
        let (low, high) = if n.big_endian() {
            (len * 8 - 1 - lsb, len * 8 - 1 - msb)
        } else {
            (lsb, msb)
        };
        ensure!(low <= high, "invalid masked register bit order");
        Ok((low, high - low + 1))
    }
    fn enum_entries<'a>(&'a self, n: &'a Node) -> Vec<&'a str> {
        n.props
            .iter()
            .filter_map(|p| {
                if p.tag == "pEnumEntry" {
                    Some(p.text.as_str())
                } else if p.tag == "EnumEntry" {
                    p.attrs.get("Name").map(String::as_str)
                } else {
                    None
                }
            })
            .collect()
    }
    pub fn set(&self, io: &mut dyn RegisterIo, name: &str, value: &str) -> Result<()> {
        let n = self.node(name, 0)?;
        let v = match n.kind.as_str() {
            "String" | "StringReg" => json!(value),
            "Boolean" => match value.to_ascii_lowercase().as_str() {
                "true" | "1" | "on" => json!(true),
                "false" | "0" | "off" => json!(false),
                _ => bail!("boolean must be true or false"),
            },
            "Enumeration" => json!(value),
            "Command" => bail!("use execute for command {name}"),
            _ => literal(value)?,
        };
        self.set_inner(io, name, &v, 0)
    }
    fn set_inner(
        &self,
        io: &mut dyn RegisterIo,
        name: &str,
        value: &Value,
        depth: usize,
    ) -> Result<()> {
        self.access(io, name, true, depth)?;
        let n = self.node(name, depth)?;
        ensure!(
            matches!(
                n.kind.as_str(),
                "Integer"
                    | "Float"
                    | "String"
                    | "Boolean"
                    | "Enumeration"
                    | "Converter"
                    | "IntConverter"
                    | "IntReg"
                    | "MaskedIntReg"
                    | "FloatReg"
                    | "StringReg"
            ),
            "unsupported writable node type {}",
            n.kind
        );
        let translated = match n.kind.as_str() {
            "Boolean" => {
                let on = value.as_bool().context("expected boolean")?;
                let key = if on { "OnValue" } else { "OffValue" };
                if let Some(link) = n.prop(&format!("p{key}")) {
                    self.get_inner(io, link, depth + 1)?
                } else {
                    literal(n.prop(key).unwrap_or(if on { "1" } else { "0" }))?
                }
            }
            "Enumeration" => {
                let symbol = value.as_str().context("expected enumeration symbol")?;
                let entry = self
                    .enum_entries(n)
                    .into_iter()
                    .find(|entry| {
                        self.nodes.get(*entry).is_some_and(|en| {
                            symbol == *entry
                                || symbol
                                    == en.prop("Symbolic").unwrap_or_else(|| {
                                        entry.strip_prefix(&format!("{name}_")).unwrap_or(entry)
                                    })
                        })
                    })
                    .ok_or_else(|| anyhow!("unknown {name} choice {symbol}"))?;
                let en = self.node(entry, depth + 1)?;
                self.available(io, en, depth + 1)?;
                if let Some(link) = en.prop("pValue") {
                    self.get_inner(io, link, depth + 1)?
                } else {
                    literal(en.prop("Value").context("enumeration entry has no value")?)?
                }
            }
            "IntConverter" | "Converter" => {
                let v = self.formula(io, n, "FormulaTo", Some(("FROM", number(value)?)), depth)?;
                if n.kind == "IntConverter" {
                    integer_formula(v)?
                } else {
                    json!(v)
                }
            }
            _ => value.clone(),
        };
        if !matches!(
            n.kind.as_str(),
            "String" | "StringReg" | "Boolean" | "Enumeration"
        ) {
            let v = number(value)?;
            for (key, cmp) in [("Min", true), ("Max", false)] {
                if let Some(bound) = self.property_number(io, n, key, depth)? {
                    ensure!(
                        if cmp { v >= bound } else { v <= bound },
                        "{name} violates {key}={bound}"
                    );
                }
            }
            if let Some(inc) = self.property_number(io, n, "Inc", depth)? {
                ensure!(inc > 0.0, "{name} has invalid increment");
                let base = self.property_number(io, n, "Min", depth)?.unwrap_or(0.0);
                let steps = (v - base) / inc;
                ensure!(
                    (steps - steps.round()).abs() < 1e-7,
                    "{name} requires increment {inc} from {base}"
                );
            }
            if matches!(
                n.kind.as_str(),
                "Integer" | "IntReg" | "MaskedIntReg" | "IntConverter"
            ) {
                ensure!(v.fract() == 0.0, "{name} requires an integer");
            }
        }
        if let Some(link) = self.resolve_value_link(io, n, depth)? {
            let translated = if n.kind == "Converter" && self.integer_target(link, depth + 1)? {
                let numeric = number(&translated)?;
                ensure!(
                    numeric.abs() <= (1u64 << 53) as f64,
                    "converter output exceeds exact integer conversion range"
                );
                json!(numeric.trunc() as i64)
            } else {
                translated
            };
            return self.set_inner(io, link, &translated, depth + 1);
        }
        if n.host_value() {
            n.stored.replace(Some(if n.kind == "Integer" {
                json!(signed_integer(&translated)?)
            } else {
                translated
            }));
            return Ok(());
        }
        let (addr, len) = self.register(io, n, depth)?;
        let bytes = match n.kind.as_str() {
            "IntReg" | "MaskedIntReg" => {
                ensure!(
                    matches!(len, 1 | 2 | 4 | 8),
                    "unsupported integer register size"
                );
                let (shift, bits) = if n.kind == "MaskedIntReg" {
                    self.mask(n, len)?
                } else {
                    (0, len * 8)
                };
                let raw = if n.prop("Sign") == Some("Signed") {
                    let i = signed_integer(&translated)?;
                    if bits < 64 {
                        ensure!(
                            i >= -(1i64 << (bits - 1)) && i < (1i64 << (bits - 1)),
                            "signed register value out of range"
                        );
                    }
                    (i as u64) & bit_mask(bits)
                } else {
                    let u = unsigned_integer(&translated)?;
                    ensure!(u <= bit_mask(bits), "register value out of range");
                    u
                };
                let raw = if n.kind == "MaskedIntReg" {
                    self.access(io, name, false, depth)?;
                    let old = io.read_memory(addr, len)?;
                    ensure!(old.len() == len, "short masked register read");
                    (decode_integer(&old, n.big_endian()) & !(bit_mask(bits) << shift))
                        | (raw << shift)
                } else {
                    raw
                };
                encode_integer(raw, len, n.big_endian())
            }
            "FloatReg" => {
                let v = number(&translated)?;
                match len {
                    4 => {
                        ensure!((v as f32).is_finite(), "value exceeds float32 range");
                        encode_integer((v as f32).to_bits() as u64, 4, n.big_endian())
                    }
                    8 => encode_integer(v.to_bits(), 8, n.big_endian()),
                    _ => bail!("unsupported float register size"),
                }
            }
            "StringReg" => {
                let s = translated.as_str().context("expected string")?;
                ensure!(
                    !s.contains('\0') && s.len() < len,
                    "string needs room for a NUL terminator ({len} bytes)"
                );
                let mut out = vec![0; len];
                out[..s.len()].copy_from_slice(s.as_bytes());
                out
            }
            _ => bail!("{name} has no supported writable register"),
        };
        io.write_memory(addr, &bytes)
    }
    fn integer_target(&self, name: &str, depth: usize) -> Result<bool> {
        let node = self.node(name, depth)?;
        match node.kind.as_str() {
            "Integer" | "IntReg" | "MaskedIntReg" | "IntConverter" => Ok(true),
            "Float" | "Converter" => {
                if let Some(link) = node.prop("pValue") {
                    self.integer_target(link, depth + 1)
                } else {
                    Ok(false)
                }
            }
            _ => Ok(false),
        }
    }
    pub fn execute(&self, io: &mut dyn RegisterIo, name: &str) -> Result<()> {
        self.access(io, name, true, 0)?;
        let n = self.node(name, 0)?;
        ensure!(n.kind == "Command", "{name} is not a command");
        let v = if let Some(link) = n.prop("pCommandValue") {
            self.get_inner(io, link, 1)?
        } else {
            literal(
                n.prop("CommandValue")
                    .context("command has no CommandValue")?,
            )?
        };
        self.set_inner(
            io,
            n.prop("pValue").context("command has no pValue")?,
            &v,
            1,
        )
    }
    fn formula(
        &self,
        io: &mut dyn RegisterIo,
        n: &Node,
        key: &str,
        special: Option<(&str, f64)>,
        depth: usize,
    ) -> Result<f64> {
        let mut vars = BTreeMap::new();
        for p in &n.props {
            if matches!(p.tag.as_str(), "pVariable" | "Constant" | "Expression") {
                let name = p
                    .attrs
                    .get("Name")
                    .context("formula variable missing Name")?;
                let v = if p.tag == "pVariable" {
                    self.number(io, &p.text, depth + 1)?
                } else if p.tag == "Constant" {
                    number(&literal(&p.text)?)?
                } else {
                    evaluate(&p.text, &vars)?
                };
                if matches!(n.kind.as_str(), "IntSwissKnife" | "IntConverter") {
                    ensure!(
                        v.abs() <= (1u64 << 53) as f64,
                        "integer formula exceeds exact arithmetic range"
                    );
                }
                vars.insert(name.clone(), v);
            }
        }
        if let Some((name, value)) = special {
            vars.insert(name.into(), value);
        }
        evaluate(
            n.prop(key).ok_or_else(|| anyhow!("node missing {key}"))?,
            &vars,
        )
    }
}
fn fields_has(fields: &[Element], tag: &str) -> bool {
    fields.iter().any(|p| p.tag == tag)
}
fn bit_mask(bits: usize) -> u64 {
    if bits == 64 {
        u64::MAX
    } else {
        (1u64 << bits) - 1
    }
}
fn decode_integer(b: &[u8], big: bool) -> u64 {
    if big {
        b.iter().fold(0, |v, b| (v << 8) | *b as u64)
    } else {
        b.iter()
            .enumerate()
            .fold(0, |v, (i, b)| v | ((*b as u64) << (i * 8)))
    }
}
fn encode_integer(v: u64, len: usize, big: bool) -> Vec<u8> {
    if big {
        v.to_be_bytes()[8 - len..].to_vec()
    } else {
        v.to_le_bytes()[..len].to_vec()
    }
}
fn number(v: &Value) -> Result<f64> {
    let n = v
        .as_f64()
        .or_else(|| v.as_bool().map(|b| if b { 1.0 } else { 0.0 }))
        .context("expected numeric value")?;
    ensure!(n.is_finite(), "expected finite number");
    Ok(n)
}
fn signed_integer(v: &Value) -> Result<i64> {
    if let Some(value) = v.as_i64() {
        return Ok(value);
    }
    let n = number(v)?;
    ensure!(
        n.fract() == 0.0 && n.abs() <= (1u64 << 53) as f64,
        "expected exact signed integer"
    );
    Ok(n as i64)
}
fn unsigned_integer(v: &Value) -> Result<u64> {
    if let Some(value) = v.as_u64() {
        return Ok(value);
    }
    let n = number(v)?;
    ensure!(
        n.fract() == 0.0 && n >= 0.0 && n <= (1u64 << 53) as f64,
        "expected exact unsigned integer"
    );
    Ok(n as u64)
}
fn unsigned(v: &Value) -> Result<u64> {
    v.as_u64()
        .ok_or_else(|| anyhow!("expected unsigned integer"))
}
fn literal(s: &str) -> Result<Value> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        return Ok(json!(
            u64::from_str_radix(hex, 16).context("invalid hexadecimal number")?
        ));
    }
    if let Some(hex) = s.strip_prefix("-0x").or_else(|| s.strip_prefix("-0X")) {
        return Ok(json!(
            i64::from_str_radix(hex, 16)
                .context("invalid hexadecimal number")?
                .checked_neg()
                .context("integer overflow")?
        ));
    }
    if let Ok(i) = s.parse::<i64>() {
        return Ok(json!(i));
    }
    if let Ok(u) = s.parse::<u64>() {
        return Ok(json!(u));
    }
    let n: f64 = s.parse().context("invalid numeric value")?;
    ensure!(n.is_finite(), "expected finite number");
    Ok(json!(n))
}
fn integer_formula(v: f64) -> Result<Value> {
    ensure!(
        v.is_finite() && v.fract() == 0.0 && v.abs() <= (1u64 << 53) as f64,
        "integer formula result cannot be represented exactly"
    );
    Ok(json!(v as i64))
}

/// Decode a raw XML file or an in-memory ZIP, without writing device-provided paths.
pub fn decode_xml(data: &[u8]) -> Result<String> {
    ensure!(data.len() <= MAX_XML, "GenICam file exceeds 32 MiB limit");
    let bytes = if data.starts_with(b"PK\x03\x04") {
        let mut archive = zip::ZipArchive::new(Cursor::new(data)).context("invalid GenICam ZIP")?;
        let indices = (0..archive.len())
            .filter(|i| {
                archive
                    .by_index(*i)
                    .map(|f| f.name().to_ascii_lowercase().ends_with(".xml") && !f.is_dir())
                    .unwrap_or(false)
            })
            .collect::<Vec<_>>();
        ensure!(
            indices.len() == 1,
            "GenICam ZIP must contain exactly one XML file"
        );
        let index = indices[0];
        let file = archive.by_index(index)?;
        ensure!(
            file.size() <= MAX_XML as u64,
            "uncompressed XML exceeds 32 MiB limit"
        );
        let mut bytes = Vec::new();
        file.take(MAX_XML as u64 + 1).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= MAX_XML,
            "uncompressed XML exceeds 32 MiB limit"
        );
        bytes
    } else {
        data.to_vec()
    };
    let xml = String::from_utf8(bytes).context("GenICam XML is not UTF-8")?;
    Ok(xml
        .trim_start_matches('\u{feff}')
        .trim_end_matches('\0')
        .to_string())
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Num(f64),
    Name(String),
    Op(String),
    L,
    R,
    Q,
    Colon,
    End,
}
fn evaluate(expression: &str, vars: &BTreeMap<String, f64>) -> Result<f64> {
    ensure!(expression.len() <= 16384, "formula exceeds size limit");
    ensure!(
        expression.is_ascii(),
        "non-ASCII formula syntax is unsupported"
    );
    let mut tokens = Vec::new();
    let bytes = expression.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_whitespace() {
            i += 1;
            continue;
        }
        let start = i;
        if bytes[i].is_ascii_digit() || bytes[i] == b'.' {
            i += 1;
            if bytes[start] == b'0' && i < bytes.len() && matches!(bytes[i], b'x' | b'X') {
                i += 1;
                while i < bytes.len() && bytes[i].is_ascii_hexdigit() {
                    i += 1;
                }
            } else {
                while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
                    i += 1;
                }
                if i < bytes.len() && matches!(bytes[i], b'e' | b'E') {
                    i += 1;
                    if i < bytes.len() && matches!(bytes[i], b'+' | b'-') {
                        i += 1;
                    }
                    while i < bytes.len() && bytes[i].is_ascii_digit() {
                        i += 1;
                    }
                }
            }
            tokens.push(Token::Num(number(&literal(&expression[start..i])?)?));
            continue;
        }
        if bytes[i].is_ascii_alphabetic() || bytes[i] == b'_' {
            i += 1;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            tokens.push(Token::Name(expression[start..i].into()));
            continue;
        }
        if i + 1 < bytes.len()
            && matches!(
                &expression[i..i + 2],
                "<<" | ">>" | "<=" | ">=" | "==" | "!=" | "<>" | "&&" | "||" | "**"
            )
        {
            tokens.push(Token::Op(expression[i..i + 2].into()));
            i += 2;
            continue;
        }
        tokens.push(match bytes[i] {
            b'(' => Token::L,
            b')' => Token::R,
            b'?' => Token::Q,
            b':' => Token::Colon,
            b'+' | b'-' | b'*' | b'/' | b'%' | b'&' | b'|' | b'^' | b'~' | b'!' | b'<' | b'>'
            | b'=' => Token::Op((bytes[i] as char).to_string()),
            _ => bail!("unsupported formula syntax at byte {i}"),
        });
        i += 1;
    }
    tokens.push(Token::End);
    let mut parser = Expr {
        tokens,
        pos: 0,
        vars,
        depth: 0,
    };
    let value = parser.expr(0)?;
    ensure!(
        parser.tokens[parser.pos] == Token::End,
        "unsupported trailing formula syntax"
    );
    ensure!(value.is_finite(), "formula produced non-finite result");
    Ok(value)
}
struct Expr<'a> {
    tokens: Vec<Token>,
    pos: usize,
    vars: &'a BTreeMap<String, f64>,
    depth: usize,
}
impl Expr<'_> {
    fn expr(&mut self, min: u8) -> Result<f64> {
        self.depth += 1;
        ensure!(self.depth <= MAX_DEPTH, "formula nesting exceeds limit");
        let token = self.tokens[self.pos].clone();
        self.pos += 1;
        let mut lhs = match token {
            Token::Num(n) => n,
            Token::Name(n) => {
                if self.tokens[self.pos] == Token::L {
                    self.pos += 1;
                    let arg = self.expr(0)?;
                    ensure!(
                        self.tokens[self.pos] == Token::R,
                        "unclosed formula function"
                    );
                    self.pos += 1;
                    match n.as_str() {
                        "ABS" => arg.abs(),
                        "EXP" => arg.exp(),
                        "LN" => arg.ln(),
                        "LOG" => arg.log10(),
                        "SQRT" => arg.sqrt(),
                        "SIN" => arg.sin(),
                        "COS" => arg.cos(),
                        "TAN" => arg.tan(),
                        "ASIN" => arg.asin(),
                        "ACOS" => arg.acos(),
                        "ATAN" => arg.atan(),
                        "FLOOR" => arg.floor(),
                        "CEIL" => arg.ceil(),
                        "ROUND" => arg.round(),
                        "TRUNC" => arg.trunc(),
                        _ => bail!("unsupported formula function {n}"),
                    }
                } else {
                    *self
                        .vars
                        .get(&n)
                        .ok_or_else(|| anyhow!("unknown formula variable {n}"))?
                }
            }
            Token::L => {
                let v = self.expr(0)?;
                ensure!(
                    self.tokens[self.pos] == Token::R,
                    "unclosed formula parenthesis"
                );
                self.pos += 1;
                v
            }
            Token::Op(op) if matches!(op.as_str(), "+" | "-" | "!" | "~") => {
                let v = self.expr(12)?;
                match op.as_str() {
                    "+" => v,
                    "-" => -v,
                    "!" => (v == 0.0) as u8 as f64,
                    _ => (!exact_i64(v)?) as f64,
                }
            }
            _ => bail!("expected formula operand"),
        };
        loop {
            if self.tokens[self.pos] == Token::Q && min == 0 {
                self.pos += 1;
                let yes = self.expr(0)?;
                ensure!(
                    self.tokens[self.pos] == Token::Colon,
                    "expected ':' in formula"
                );
                self.pos += 1;
                let no = self.expr(0)?;
                lhs = if lhs != 0.0 { yes } else { no };
                continue;
            }
            let Token::Op(op) = &self.tokens[self.pos] else {
                break;
            };
            let prec = match op.as_str() {
                "||" => 1,
                "&&" => 2,
                "|" => 3,
                "^" => 4,
                "&" => 5,
                "==" | "=" | "!=" | "<>" => 6,
                "<" | ">" | "<=" | ">=" => 7,
                "<<" | ">>" => 8,
                "+" | "-" => 9,
                "*" | "/" | "%" => 10,
                "**" => 11,
                _ => break,
            };
            if prec < min {
                break;
            }
            let op = op.clone();
            self.pos += 1;
            let rhs = self.expr(if op == "**" { prec } else { prec + 1 })?;
            lhs = match op.as_str() {
                "+" => lhs + rhs,
                "-" => lhs - rhs,
                "*" => lhs * rhs,
                "/" => {
                    ensure!(rhs != 0.0, "formula division by zero");
                    lhs / rhs
                }
                "%" => {
                    ensure!(rhs != 0.0, "formula modulo by zero");
                    lhs % rhs
                }
                "**" => lhs.powf(rhs),
                "&" => (exact_i64(lhs)? & exact_i64(rhs)?) as f64,
                "|" => (exact_i64(lhs)? | exact_i64(rhs)?) as f64,
                "^" => (exact_i64(lhs)? ^ exact_i64(rhs)?) as f64,
                "<<" | ">>" => {
                    let shift = exact_i64(rhs)?;
                    ensure!((0..64).contains(&shift), "invalid formula shift");
                    if op == "<<" {
                        exact_i64(lhs)?
                            .checked_shl(shift as u32)
                            .context("formula shift overflow")? as f64
                    } else {
                        (exact_i64(lhs)? >> shift) as f64
                    }
                }
                "==" | "=" => (lhs == rhs) as u8 as f64,
                "!=" | "<>" => (lhs != rhs) as u8 as f64,
                "<" => (lhs < rhs) as u8 as f64,
                ">" => (lhs > rhs) as u8 as f64,
                "<=" => (lhs <= rhs) as u8 as f64,
                ">=" => (lhs >= rhs) as u8 as f64,
                "&&" => (lhs != 0.0 && rhs != 0.0) as u8 as f64,
                "||" => (lhs != 0.0 || rhs != 0.0) as u8 as f64,
                _ => unreachable!(),
            };
            ensure!(lhs.is_finite(), "non-finite formula result");
        }
        self.depth -= 1;
        Ok(lhs)
    }
}
fn exact_i64(v: f64) -> Result<i64> {
    ensure!(
        v.fract() == 0.0 && v.abs() <= (1u64 << 53) as f64,
        "bitwise formula operand outside exact integer range"
    );
    Ok(v as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct Memory {
        bytes: BTreeMap<u64, u8>,
    }
    impl RegisterIo for Memory {
        fn read_memory(&mut self, a: u64, n: usize) -> Result<Vec<u8>> {
            Ok((0..n)
                .map(|i| *self.bytes.get(&(a + i as u64)).unwrap_or(&0))
                .collect())
        }
        fn write_memory(&mut self, a: u64, b: &[u8]) -> Result<()> {
            for (i, v) in b.iter().enumerate() {
                self.bytes.insert(a + i as u64, *v);
            }
            Ok(())
        }
    }
    fn map(body: &str) -> NodeMap {
        NodeMap::parse(&format!(
            "<RegisterDescription>{body}</RegisterDescription>"
        ))
        .unwrap()
    }
    #[test]
    fn scoped_enumeration_entries_and_grouped_features() {
        let m = map(
            "<Group><Integer Name='Width'><Value>640</Value></Integer><Enumeration Name='Selector'><EnumEntry Name='Start'><Value>0</Value></EnumEntry><Value>0</Value></Enumeration><Enumeration Name='Other'><EnumEntry Name='Start'><Value>1</Value></EnumEntry><Value>1</Value></Enumeration></Group><Command Name='Start'><pValue>R</pValue><CommandValue>1</CommandValue></Command><IntReg Name='R'><Address>0</Address><Length>4</Length></IntReg><StructReg><Address>4</Address><Length>4</Length><StructEntry Name='StructBit'><Bit>0</Bit></StructEntry></StructReg>",
        );
        let mut io = Memory::default();
        assert!(m.has("Width"));
        assert!(m.has("StructBit"));
        assert_eq!(m.get(&mut io, "Selector").unwrap(), "Start");
        assert_eq!(m.get(&mut io, "Other").unwrap(), "Start");
        m.execute(&mut io, "Start").unwrap();
    }
    #[test]
    fn registers_bounds_and_selectors() {
        let m = map(
            "<Integer Name='Width'><pValue>Reg</pValue><Min>2</Min><Max>64</Max><Inc>2</Inc></Integer><IntReg Name='Reg'><Address>0x10</Address><pAddress>Offset</pAddress><Length>4</Length><AccessMode>RW</AccessMode><Endianess>BigEndian</Endianess></IntReg><IntSwissKnife Name='Offset'><Formula>S * 4</Formula><pVariable Name='S'>Selector</pVariable></IntSwissKnife><Integer Name='Selector'><Value>3</Value></Integer>",
        );
        let mut io = Memory::default();
        m.set(&mut io, "Width", "32").unwrap();
        assert_eq!(io.read_memory(0x1c, 4).unwrap(), [0, 0, 0, 32]);
        assert_eq!(m.get(&mut io, "Width").unwrap(), 32);
        assert!(m.set(&mut io, "Width", "33").is_err());
        assert!(m.set(&mut io, "Width", "66").is_err());
    }
    #[test]
    fn indexed_value_routes_read_write_and_default_without_changing_register_indexing() {
        let m = map(
            "<Integer Name='Selector'><pValue>S</pValue></Integer><IntReg Name='S'><Address>0</Address><Length>4</Length></IntReg><Integer Name='Mux'><pIndex>Selector</pIndex><pValueIndexed Index='1'>A</pValueIndexed><pValueIndexed Index='2'>B</pValueIndexed><pValueDefault>Default</pValueDefault></Integer><IntReg Name='A'><Address>16</Address><Length>4</Length></IntReg><IntReg Name='B'><Address>20</Address><Length>4</Length><AccessMode>RO</AccessMode></IntReg><IntReg Name='Default'><Address>32</Address><pIndex Offset='4'>Selector</pIndex><Length>4</Length></IntReg>",
        );
        let mut io = Memory::default();
        m.set(&mut io, "Selector", "1").unwrap();
        m.set(&mut io, "Mux", "42").unwrap();
        assert_eq!(m.get(&mut io, "Mux").unwrap(), 42);
        assert_eq!(io.read_memory(16, 4).unwrap(), 42u32.to_le_bytes());
        m.set(&mut io, "Selector", "2").unwrap();
        assert!(m.set(&mut io, "Mux", "43").is_err());
        assert!(m.writable(&mut io, "Mux", 0).is_err());
        m.set(&mut io, "Selector", "3").unwrap();
        m.set(&mut io, "Mux", "44").unwrap();
        assert_eq!(m.get(&mut io, "Mux").unwrap(), 44);
        assert_eq!(io.read_memory(44, 4).unwrap(), 44u32.to_le_bytes());
        let missing = map(
            "<Integer Name='S'><Value>3</Value></Integer><Integer Name='Mux'><pIndex>S</pIndex><pValueIndexed Index='1'>S</pValueIndexed></Integer>",
        );
        assert!(missing.get(&mut io, "Mux").is_err());
    }
    #[test]
    fn masked_boolean_preserves_other_bits() {
        let m = map(
            "<Boolean Name='Enabled'><pValue>Bit</pValue></Boolean><MaskedIntReg Name='Bit'><Address>0</Address><Length>4</Length><Bit>3</Bit><AccessMode>RW</AccessMode></MaskedIntReg>",
        );
        let mut io = Memory::default();
        io.write_memory(0, &[0x81, 0, 0, 0]).unwrap();
        m.set(&mut io, "Enabled", "true").unwrap();
        assert_eq!(io.read_memory(0, 4).unwrap(), [0x89, 0, 0, 0]);
        assert_eq!(m.get(&mut io, "Enabled").unwrap(), true);
    }
    #[test]
    fn big_endian_masks_follow_genapi_bit_order() {
        let m = map(
            "<MaskedIntReg Name='Bits'><Address>0</Address><Length>4</Length><Endianess>BigEndian</Endianess><LSB>23</LSB><MSB>16</MSB></MaskedIntReg>",
        );
        let mut io = Memory::default();
        io.write_memory(0, &[1, 2, 3, 4]).unwrap();
        assert_eq!(m.get(&mut io, "Bits").unwrap(), 3);
        m.set(&mut io, "Bits", "170").unwrap();
        assert_eq!(io.read_memory(0, 4).unwrap(), [1, 2, 170, 4]);
    }
    #[test]
    fn zip_xml_limits_and_ambiguity() {
        use std::io::Write;
        let archive = |names: &[&str]| {
            let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
            for name in names {
                zip.start_file(*name, zip::write::SimpleFileOptions::default())
                    .unwrap();
                zip.write_all(b"<RegisterDescription/>").unwrap();
            }
            zip.finish().unwrap().into_inner()
        };
        assert_eq!(
            decode_xml(&archive(&["model.xml"])).unwrap(),
            "<RegisterDescription/>"
        );
        assert!(decode_xml(&archive(&["a.xml", "b.xml"])).is_err());
        assert!(decode_xml(&archive(&["readme.txt"])).is_err());
    }
    #[test]
    fn unavailable_and_locked_features_fail_closed() {
        let m = map(
            "<Integer Name='Feature'><pValue>R</pValue><pIsAvailable>Available</pIsAvailable><pIsLocked>Locked</pIsLocked></Integer><Integer Name='Available'><Value>1</Value></Integer><Integer Name='Locked'><Value>1</Value></Integer><IntReg Name='R'><Address>0</Address><Length>4</Length></IntReg><Integer Name='Unavailable'><pValue>R</pValue><pIsAvailable>No</pIsAvailable></Integer><Integer Name='No'><Value>0</Value></Integer>",
        );
        let mut io = Memory::default();
        assert!(m.get(&mut io, "Feature").is_ok());
        assert!(m.set(&mut io, "Feature", "3").is_err());
        assert!(m.get(&mut io, "Unavailable").is_err());
    }
    #[test]
    fn enumeration_command_and_access() {
        let m = map(
            "<Enumeration Name='Mode'><pValue>R</pValue><EnumEntry Name='A'><Value>1</Value></EnumEntry><EnumEntry Name='B'><Value>2</Value></EnumEntry></Enumeration><IntReg Name='R'><Address>4</Address><Length>4</Length><AccessMode>RW</AccessMode></IntReg><Command Name='Start'><pValue>R</pValue><CommandValue>1</CommandValue></Command><Integer Name='ReadOnly'><pValue>R</pValue><ImposedAccessMode>RO</ImposedAccessMode></Integer>",
        );
        let mut io = Memory::default();
        m.set(&mut io, "Mode", "B").unwrap();
        assert_eq!(m.get(&mut io, "Mode").unwrap(), "B");
        m.execute(&mut io, "Start").unwrap();
        assert_eq!(m.get(&mut io, "Mode").unwrap(), "A");
        assert!(m.set(&mut io, "ReadOnly", "2").is_err());
    }
    #[test]
    fn converter_float_and_cycles() {
        let m = map(
            "<Float Name='Exposure'><pValue>C</pValue></Float><Converter Name='C'><pValue>R</pValue><FormulaFrom>TO * 0.5</FormulaFrom><FormulaTo>FROM / 0.5</FormulaTo></Converter><FloatReg Name='R'><Address>0</Address><Length>8</Length></FloatReg><Integer Name='Loop'><pValue>Loop</pValue></Integer>",
        );
        let mut io = Memory::default();
        m.set(&mut io, "Exposure", "100").unwrap();
        assert_eq!(m.get(&mut io, "Exposure").unwrap(), 100.0);
        assert!(m.get(&mut io, "Loop").is_err());
        assert_eq!(m.bounds(&mut io, "Loop").unwrap(), Bounds::default());
    }
    #[test]
    fn formula_fail_closed() {
        let vars = BTreeMap::from([("X".into(), 7.0)]);
        assert_eq!(
            evaluate("(X & 3) == 3 ? 0x10 + 2 * 4 : 0", &vars).unwrap(),
            24.0
        );
        assert!(evaluate("UNKNOWN(X)", &vars).is_err());
        assert_eq!(evaluate("X=7 ? EXP(LN(1)) : 0", &vars).unwrap(), 1.0);
        assert!(evaluate("X / 0", &vars).is_err());
        assert!(evaluate("X + ", &vars).is_err());
    }
    #[test]
    fn unsigned_64_and_signed_registers() {
        let m = map(
            "<IntReg Name='U'><Address>0</Address><Length>8</Length></IntReg><IntReg Name='S'><Address>8</Address><Length>2</Length><Sign>Signed</Sign><Endianess>BigEndian</Endianess></IntReg>",
        );
        let mut io = Memory::default();
        m.set(&mut io, "U", "18446744073709551615").unwrap();
        assert_eq!(m.get(&mut io, "U").unwrap(), json!(u64::MAX));
        m.set(&mut io, "S", "-2").unwrap();
        assert_eq!(m.get(&mut io, "S").unwrap(), -2);
        assert_eq!(io.read_memory(8, 2).unwrap(), [255, 254]);
    }
    fn basler() -> NodeMap {
        let mut xml = concat!(
            "<Float Name='ExposureTime'><pValue>d_237</pValue></Float>",
            "<Float Name='d_237'><pValue>d_236</pValue></Float>",
            "<Integer Name='d_236'><pValue>d_155</pValue><pMin>d_1238</pMin><pMax>d_1240</pMax><Inc>1</Inc></Integer>",
            "<Float Name='Gain'><pValue>d_234</pValue></Float>",
            "<Converter Name='d_234'><FormulaTo>(EXP(((FROM/20.0)*2.302585092994046)))*65536.0</FormulaTo><FormulaFrom>((LN((TO/65536.0)))/2.302585092994046)*20.0</FormulaFrom><pValue>d_230</pValue><Slope>Automatic</Slope></Converter>",
            "<Float Name='d_230'><pValue>d_229</pValue></Float>",
            "<Integer Name='d_229'><pValue>d_682</pValue><Min>65536</Min><Max>16461899</Max><Inc>1</Inc></Integer>",
            "<Enumeration Name='GainSelector'><EnumEntry Name='All'><Value>0</Value></EnumEntry><Value>0</Value></Enumeration>",
            "<Float Name='AcquisitionFrameRate'><pValue>d_244</pValue></Float>",
            "<Converter Name='d_244'><FormulaTo>FROM*10.0</FormulaTo><FormulaFrom>TO/10.0</FormulaFrom><pValue>d_243</pValue><Slope>Automatic</Slope></Converter>",
            "<Float Name='d_243'><pValue>d_242</pValue></Float>",
            "<Integer Name='d_242'><pValue>d_180</pValue><Min>1</Min><Max>10000000</Max><Inc>1</Inc></Integer>",
            "<Float Name='AutoExposureTimeLowerLimit'><pValue>d_1188</pValue></Float>",
            "<Float Name='d_1188'><pValue>d_1187</pValue></Float>",
            "<Integer Name='d_1187'><pValue>d_1331</pValue><Min>1</Min><Max>10000000</Max><Inc>1</Inc></Integer>",
            "<Float Name='AutoExposureTimeUpperLimit'><pValue>d_1190</pValue></Float>",
            "<Float Name='d_1190'><pValue>d_1189</pValue></Float>",
            "<Integer Name='d_1189'><pValue>d_1332</pValue><pMin>d_1351</pMin><Max>10000000</Max><Inc>1</Inc></Integer>",
            "<Enumeration Name='BalanceRatioSelector'><EnumEntry Name='Red'><Value>0</Value></EnumEntry><EnumEntry Name='Green'><Value>1</Value></EnumEntry><EnumEntry Name='Blue'><Value>2</Value></EnumEntry><pValue>d_1502</pValue></Enumeration>",
            "<Integer Name='d_1502'><Value>0</Value></Integer>",
            "<Float Name='BalanceRatio'><pValue>d_1506</pValue></Float>",
            "<Converter Name='d_1506'><FormulaTo>FROM*4096.0</FormulaTo><FormulaFrom>TO/4096.0</FormulaFrom><pValue>d_1505</pValue><Slope>Automatic</Slope></Converter>",
            "<Float Name='d_1505'><pValue>d_1504</pValue></Float>",
            "<Integer Name='d_1504'><pIndex>d_1502</pIndex><pValueIndexed Index='1'>d_1500</pValueIndexed><pValueIndexed Index='2'>d_1501</pValueIndexed><pValueDefault>d_1499</pValueDefault></Integer>",
            "<Integer Name='d_1499'><pValue>d_1569</pValue><Min>1024</Min><Max>65535</Max><Inc>1</Inc></Integer>",
            "<Integer Name='d_1500'><pValue>d_1570</pValue><Min>1024</Min><Max>65535</Max><Inc>1</Inc></Integer>",
            "<Integer Name='d_1501'><pValue>d_1571</pValue><Min>1024</Min><Max>65535</Max><Inc>1</Inc></Integer>",
            "<Integer Name='GevSCPSPacketSize'><pIsLocked>d_1000</pIsLocked><pValue>d_1073</pValue></Integer>",
            "<Integer Name='d_1073'><pValue>d_175</pValue><Min>500</Min><Max>8192</Max><Inc>1</Inc></Integer>",
            "<Enumeration Name='DeviceTLType'><ImposedAccessMode>RO</ImposedAccessMode><EnumEntry Name='GigEVision'><Value>0</Value></EnumEntry><Value>0</Value></Enumeration>",
        )
        .to_string();
        for (name, address) in [
            ("d_155", 0x1000_10d0),
            ("d_1238", 0x1000_10c4),
            ("d_1240", 0x1000_10c8),
            ("d_682", 0x1000_04c0),
            ("d_180", 0x1000_05e8),
            ("d_1331", 0x1000_37a4),
            ("d_1332", 0x1000_37b4),
            ("d_1351", 0x1000_37a8),
            ("d_1569", 0x1000_3f30),
            ("d_1570", 0x1000_3f40),
            ("d_1571", 0x1000_3f50),
            ("d_1000", 0x1000_8a68),
            ("d_175", 0x1000_1954),
        ] {
            xml += &format!(
                "<IntReg Name='{name}'><Address>{address:#x}</Address><Length>4</Length><Endianess>BigEndian</Endianess></IntReg>"
            );
        }
        map(&xml)
    }
    fn bounds(min: f64, max: f64, inc: Option<f64>) -> Bounds {
        Bounds {
            min: Some(min),
            max: Some(max),
            inc,
        }
    }
    #[test]
    fn bounds_follow_value_chains_through_converters() {
        let m = basler();
        let mut io = Memory::default();
        io.write_memory(0x1000_10c4, &19u32.to_be_bytes()).unwrap();
        io.write_memory(0x1000_10c8, &10_000_000u32.to_be_bytes())
            .unwrap();
        assert_eq!(
            m.bounds(&mut io, "ExposureTime").unwrap(),
            bounds(19.0, 1e7, Some(1.0))
        );
        let gain = m.bounds(&mut io, "Gain").unwrap();
        assert_eq!((gain.min, gain.inc), (Some(0.0), None));
        assert!((gain.max.unwrap() - 48.00000004350822).abs() < 1e-9);
        assert_eq!(
            m.bounds(&mut io, "AcquisitionFrameRate").unwrap(),
            bounds(0.1, 1e6, None)
        );
        assert_eq!(
            m.bounds(&mut io, "GevSCPSPacketSize").unwrap(),
            bounds(500.0, 8192.0, Some(1.0))
        );
        assert_eq!(m.bounds(&mut io, "d_175").unwrap(), Bounds::default());
        let exposure = m
            .features(&mut io)
            .into_iter()
            .find(|f| f.name == "ExposureTime")
            .unwrap();
        assert_eq!(
            (exposure.min, exposure.max, exposure.inc),
            (Some(19.0), Some(1e7), Some(1.0))
        );
        let decreasing = map(
            "<Float Name='AcquisitionFrameRate'><pValue>C</pValue></Float><Converter Name='C'><FormulaTo>(1000000 / FROM)</FormulaTo><FormulaFrom>(1000000 / TO)</FormulaFrom><pValue>P</pValue></Converter><Integer Name='P'><pValue>R</pValue><Min>1000</Min><Max>10000000</Max></Integer><IntReg Name='R'><Address>0</Address><Length>4</Length></IntReg>",
        );
        assert_eq!(
            decreasing.bounds(&mut io, "AcquisitionFrameRate").unwrap(),
            bounds(0.1, 1000.0, None)
        );
        let indexed = map(
            "<Integer Name='S'><Value>0</Value></Integer><Integer Name='M'><pIndex>S</pIndex><pValueIndexed Index='1'>B</pValueIndexed><pValueDefault>A</pValueDefault></Integer><Integer Name='A'><Value>0</Value><Min>0</Min><Max>10</Max><Inc>1</Inc></Integer><Integer Name='B'><Value>8</Value><Min>5</Min><Max>50</Max><Inc>3</Inc></Integer>",
        );
        assert_eq!(
            indexed.bounds(&mut io, "M").unwrap(),
            bounds(0.0, 10.0, Some(1.0))
        );
        indexed.set(&mut io, "S", "1").unwrap();
        assert_eq!(
            indexed.bounds(&mut io, "M").unwrap(),
            bounds(5.0, 50.0, Some(3.0))
        );
        assert!(indexed.set(&mut io, "M", "53").is_err());
        indexed.set(&mut io, "M", "11").unwrap();
        assert_eq!(indexed.get(&mut io, "M").unwrap(), 11);
        assert_eq!(indexed.get(&mut io, "A").unwrap(), 0);
    }
    #[test]
    fn dynamic_minimum_follows_the_lower_limit() {
        let m = basler();
        let mut io = Memory::default();
        for lower in [500, 20_000] {
            m.set(&mut io, "AutoExposureTimeLowerLimit", &lower.to_string())
                .unwrap();
            let mirrored = io.read_memory(0x1000_37a4, 4).unwrap();
            io.write_memory(0x1000_37a8, &mirrored).unwrap();
            assert_eq!(
                m.bounds(&mut io, "AutoExposureTimeUpperLimit").unwrap(),
                bounds(lower as f64, 1e7, Some(1.0))
            );
        }
        assert!(
            m.set(&mut io, "AutoExposureTimeUpperLimit", "10000")
                .is_err()
        );
        m.set(&mut io, "AutoExposureTimeUpperLimit", "30000")
            .unwrap();
        assert_eq!(
            io.read_memory(0x1000_37b4, 4).unwrap(),
            30_000u32.to_be_bytes()
        );
    }
    #[test]
    fn host_side_selectors_are_writable_and_route_indexed_values() {
        fn send<T: Send>(_: &T) {}
        let m = basler();
        send(&m);
        let mut io = Memory::default();
        for (address, raw) in [
            (0x1000_3f30, 4096u32),
            (0x1000_3f40, 4420),
            (0x1000_3f50, 6321),
        ] {
            io.write_memory(address, &raw.to_be_bytes()).unwrap();
        }
        assert!(m.is_writable(&mut io, "BalanceRatioSelector"));
        assert!(m.is_writable(&mut io, "GainSelector"));
        assert!(!m.is_writable(&mut io, "DeviceTLType"));
        assert!(m.set(&mut io, "DeviceTLType", "GigEVision").is_err());
        m.set(&mut io, "GainSelector", "All").unwrap();
        assert_eq!(m.get(&mut io, "GainSelector").unwrap(), "All");
        assert_eq!(m.get(&mut io, "BalanceRatioSelector").unwrap(), "Red");
        assert_eq!(m.get(&mut io, "BalanceRatio").unwrap(), 1.0);
        m.set(&mut io, "BalanceRatioSelector", "Blue").unwrap();
        assert_eq!(m.get(&mut io, "BalanceRatioSelector").unwrap(), "Blue");
        assert_eq!(m.get(&mut io, "BalanceRatio").unwrap(), 6321.0 / 4096.0);
        assert_eq!(
            m.bounds(&mut io, "BalanceRatio").unwrap(),
            bounds(0.25, 15.999755859375, None)
        );
        m.set(&mut io, "d_1502", "1.0").unwrap();
        assert_eq!(m.get(&mut io, "d_1502").unwrap(), json!(1));
        m.set(&mut io, "BalanceRatio", "1.5").unwrap();
        assert_eq!(
            io.read_memory(0x1000_3f40, 4).unwrap(),
            6144u32.to_be_bytes()
        );
        assert!(
            m.features(&mut io)
                .iter()
                .any(|f| f.name == "BalanceRatioSelector" && f.writable)
        );
        assert_eq!(
            basler().get(&mut io, "BalanceRatioSelector").unwrap(),
            "Red"
        );
    }
}
