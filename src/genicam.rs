//! A small, uncached `GenApi` interpreter. Unsupported nodes and expressions fail closed.
//! Every feature operation resolves its links again, so selector-dependent addresses and
//! access conditions are evaluated against the camera's current state.
#![forbid(unsafe_code)]
#![deny(clippy::panic_in_result_fn)]
#![cfg_attr(not(test), deny(clippy::panic, clippy::unreachable))]
#![deny(clippy::missing_const_for_fn)]
use crate::types::{Categorized, ErrorKind, RegisterIo, Usage, integral_float};
use anyhow::{Context as _, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
};

mod action;
mod chunks;
pub(crate) use action::ActionDeviceKeyTarget;
pub(crate) use chunks::ChunkContext;

const MAX_XML: usize = 32 * 1024 * 1024;
const MAX_DEPTH: usize = 64;
const MAX_FORMULA_DEPTH: usize = 128;
/// 2^53: the inclusive limit of f64's consecutive integer range.
const EXACT_INTEGER_LIMIT: f64 = 9_007_199_254_740_992.0;

/// Node kinds that are features a person reads, writes or executes.
const FEATURE_KINDS: [&str; 15] = [
    "Integer",
    "Float",
    "Boolean",
    "Enumeration",
    "String",
    "Command",
    "IntReg",
    "MaskedIntReg",
    "FloatReg",
    "StringReg",
    "Register",
    "IntSwissKnife",
    "SwissKnife",
    "IntConverter",
    "Converter",
];

/// Features a camera cannot change while it streams: the image's geometry
/// and format.
pub(crate) fn stream_locked(name: &str) -> bool {
    matches!(
        name,
        "Width"
            | "Height"
            | "OffsetX"
            | "OffsetY"
            | "PixelFormat"
            | "BinningHorizontal"
            | "BinningVertical"
            | "DecimationHorizontal"
            | "DecimationVertical"
            | "VideoMode"
    )
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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

/// Whether a feature or enumeration entry exists and can be used now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Availability {
    Available,
    /// Hidden now by `pIsAvailable`; another setting may make it available.
    Unavailable,
    /// Not in the node map, or `pIsImplemented` is false.
    Absent,
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
    /// Host-side values written to floating Value, `ValueIndexed` and
    /// `ValueDefault` elements, by element position.
    stored: RefCell<BTreeMap<usize, Value>>,
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
            "Integer" | "Float" | "String" | "Boolean" | "Enumeration" | "Command"
        ) && self
            .props
            .iter()
            .any(|p| matches!(p.tag.as_str(), "Value" | "ValueIndexed" | "ValueDefault"))
    }
    /// The host value held by the element at `index`.
    fn value(&self, index: usize) -> Result<Value> {
        if let Some(stored) = self.stored.borrow().get(&index) {
            return Ok(stored.clone());
        }
        let text = &self.props[index].text;
        // A Boolean's Value is an xs:boolean.
        match (self.kind.as_str(), text.trim()) {
            ("String", _) => Ok(json!(text)),
            ("Boolean", "true") => Ok(json!(true)),
            ("Boolean", "false") => Ok(json!(false)),
            (_, text) => literal(text),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct NodeMap {
    nodes: BTreeMap<String, Node>,
    chunk_plan: chunks::Plan,
}

impl NodeMap {
    /// A finite write-only integer target, resolved without device I/O.
    pub(crate) fn action_device_key_target(&self) -> Result<ActionDeviceKeyTarget> {
        action::target(self)
    }
    /// Write the preflighted exact key without reading or checkpointing it.
    pub(crate) fn write_action_device_key(
        &self,
        io: &mut dyn RegisterIo,
        target: &ActionDeviceKeyTarget,
        key: u32,
    ) -> Result<()> {
        action::write(self, io, target, key)
    }
    /// Known vendor byte layouts require both real identity and static XML declarations.
    pub(crate) fn pixel_encodings(&self, vendor: &str) -> Vec<crate::types::PixelEncoding> {
        use crate::types::{IdsPixelFormat, LucidPixelFormat};
        let declared = |name: &str, code: u32| self.declares_pixel_format(name, code);
        if LucidPixelFormat::vendor_matches(vendor) {
            return LucidPixelFormat::ALL
                .into_iter()
                .filter(|format| declared(format.name(), format.code()))
                .map(|format| format.encoding(None))
                .collect();
        }
        if !IdsPixelFormat::vendor_matches(vendor) {
            return Vec::new();
        }
        IdsPixelFormat::ALL
            .into_iter()
            .filter(|format| declared(format.name(), format.code()))
            .map(|format| format.encoding(None))
            .collect()
    }
    /// Whether `PixelFormat` statically declares `name` with exactly `code`.
    fn declares_pixel_format(&self, symbol_name: &str, code: u32) -> bool {
        let Some(node) = self
            .nodes
            .get("PixelFormat")
            .filter(|node| node.kind == "Enumeration")
        else {
            return false;
        };
        let Some(entry) = Self::enum_entries(node).into_iter().find_map(|name| {
            let entry = self.nodes.get(name)?;
            if entry.kind != "EnumEntry" {
                return None;
            }
            let symbol = entry
                .prop("Symbolic")
                .unwrap_or_else(|| name.strip_prefix("PixelFormat_").unwrap_or(name));
            (symbol == symbol_name).then_some(entry)
        }) else {
            return false;
        };
        if entry.prop("pValue").is_some() {
            return false;
        }
        entry
            .prop("Value")
            .and_then(|value| literal(value).ok())
            .and_then(|value| value.as_u64())
            == Some(u64::from(code))
    }

    pub(crate) fn parse(xml: &str) -> Result<Self> {
        ensure!(xml.len() <= MAX_XML, "GenICam XML exceeds 32 MiB limit");
        let doc =
            roxmltree::Document::parse(xml.trim_matches('\0')).context("invalid GenICam XML")?;
        ensure!(
            doc.root_element().tag_name().name() == "RegisterDescription",
            "expected GenICam RegisterDescription root"
        );
        let mut nodes = BTreeMap::new();
        // A register's address may include IntSwissKnife elements nested in
        // it. Each becomes a node, and the register's element names it.
        let embedded = |p: roxmltree::Node<'_, '_>| {
            (p.tag_name().name() == "IntSwissKnife"
                && p.parent_element().is_some_and(|parent| {
                    matches!(
                        parent.tag_name().name(),
                        "Register"
                            | "IntReg"
                            | "MaskedIntReg"
                            | "FloatReg"
                            | "StringReg"
                            | "StructReg"
                            | "StructEntry"
                    )
                }))
            .then(|| format!("__embedded::{}", p.id().get()))
        };
        let props = |n: roxmltree::Node<'_, '_>| {
            n.children()
                .filter(roxmltree::Node::is_element)
                .map(|p| Element {
                    tag: p.tag_name().name().into(),
                    text: embedded(p).unwrap_or_else(|| p.text().unwrap_or("").trim().into()),
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
                    ("MaskedIntReg", entry_name.to_owned())
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
        for knife in doc.root_element().descendants() {
            let Some(stored_name) = embedded(knife) else {
                continue;
            };
            let node = Node {
                kind: "IntSwissKnife".into(),
                props: props(knife),
                stored: RefCell::default(),
            };
            if let Some(name) = knife.attribute("Name")
                && !nodes.contains_key(name)
            {
                nodes.insert(name.into(), node.clone());
            }
            nodes.insert(stored_name, node);
        }
        ensure!(!nodes.is_empty(), "GenICam XML has no named nodes");
        let mut map = Self {
            nodes,
            chunk_plan: chunks::Plan::default(),
        };
        map.chunk_plan = chunks::Plan::new(&map);
        Ok(map)
    }
    pub(crate) fn has(&self, name: &str) -> bool {
        self.nodes.contains_key(name)
    }
    /// The first link to a node the map does not have, as `(node, link)`:
    /// a `pValue`, `pIsLocked` or other `p` element naming no node. A
    /// device's own XML has none; a copy kept on disk that has one is not
    /// the XML the device serves.
    pub(crate) fn unresolved(&self) -> Option<(String, String)> {
        self.nodes.iter().find_map(|(name, node)| {
            node.props
                .iter()
                .filter(|p| {
                    let mut tag = p.tag.chars();
                    tag.next() == Some('p') && tag.next().is_some_and(|c| c.is_ascii_uppercase())
                })
                .find(|p| !p.text.is_empty() && !self.nodes.contains_key(&p.text))
                .map(|p| (name.clone(), p.text.clone()))
        })
    }

    /// Whether the `pIsLocked` link of `name` reads `target`, directly or
    /// through the nodes its formula references.
    pub(crate) fn locked_by(&self, name: &str, target: &str) -> bool {
        let Some(link) = self.nodes.get(name).and_then(|node| node.prop("pIsLocked")) else {
            return false;
        };
        let mut pending = vec![link.trim()];
        let mut seen = BTreeSet::new();
        while let Some(next) = pending.pop() {
            if next == target {
                return true;
            }
            if seen.len() >= 256 || !seen.insert(next) {
                continue;
            }
            if let Some(node) = self.nodes.get(next) {
                pending.extend(
                    node.props
                        .iter()
                        .filter(|prop| node_link(&prop.tag))
                        .map(|prop| prop.text.trim()),
                );
            }
        }
        false
    }

    /// The feature a `pIsLocked` on `name` waits for, such as `ExposureAuto`
    /// for `ExposureTime`: an exposed value feature whose stored value the
    /// lock condition reads. Auto switches first, then other switches.
    pub(crate) fn lock_owner(&self, name: &str) -> Option<String> {
        let inputs = self.lock_inputs(name);
        if inputs.is_empty() {
            return None;
        }
        let exposed = self.exposed_features();
        self.nodes
            .iter()
            .filter(|(candidate, node)| {
                candidate.as_str() != name
                    && !candidate.starts_with("__")
                    && exposed
                        .as_ref()
                        .is_none_or(|names| names.contains(*candidate))
                    && matches!(
                        node.kind.as_str(),
                        "Enumeration" | "Boolean" | "Integer" | "Float"
                    )
                    && self
                        .value_chain(candidate)
                        .iter()
                        .any(|link| inputs.contains(link))
            })
            .min_by_key(|&(candidate, node)| {
                (
                    !candidate.ends_with("Auto"),
                    !matches!(node.kind.as_str(), "Enumeration" | "Boolean"),
                    candidate.as_str(),
                )
            })
            .map(|(owner, _)| owner.clone())
    }
    /// The XML Visibility of a feature; `GenICam`'s default is Beginner.
    pub(crate) fn visibility(&self, name: &str) -> &str {
        self.nodes
            .get(name)
            .and_then(|node| node.prop("Visibility"))
            .unwrap_or("Beginner")
    }
    /// The category that lists a feature, other than Root.
    pub(crate) fn category(&self, name: &str) -> Option<&str> {
        let mut listing = self.nodes.iter().filter(|(_, node)| {
            node.kind == "Category"
                && node
                    .props
                    .iter()
                    .any(|p| p.tag == "pFeature" && p.text == name)
        });
        listing
            .clone()
            .find(|(category, _)| category.as_str() != "Root")
            .or_else(|| listing.next())
            .map(|(category, _)| category.as_str())
    }
    /// The access a feature's declaration allows, whatever locks hold now:
    /// `ro`, `wo` or `rw`.
    pub(crate) fn static_access(&self, name: &str) -> &'static str {
        let mut name = name;
        for _ in 0..=MAX_DEPTH {
            let Some(node) = self.nodes.get(name) else {
                return "ro";
            };
            for key in ["ImposedAccessMode", "AccessMode"] {
                match node.prop(key) {
                    Some("RO") => return "ro",
                    Some("WO") => return "wo",
                    _ => {}
                }
            }
            match node.kind.as_str() {
                "Command" => return "wo",
                "IntSwissKnife" | "SwissKnife" => return "ro",
                _ => {}
            }
            match node.prop("pValue") {
                Some(link) => name = link,
                None if node.prop("Value").is_some() || node.kind.ends_with("Reg") => return "rw",
                // A value table or converter writes through its links.
                None => return "rw",
            }
        }
        "rw"
    }
    /// A maximum the feature's declaration fixes as a number, which no
    /// other setting can move.
    pub(crate) fn static_max(&self, name: &str) -> Option<f64> {
        literal(self.nodes.get(name)?.prop("Max")?).ok()?.as_f64()
    }
    /// The node's `GenICam` kind, such as `Float` or `IntReg`.
    pub(crate) fn kind(&self, name: &str) -> Option<&str> {
        self.nodes.get(name).map(|node| node.kind.as_str())
    }
    pub(crate) fn unit(&self, name: &str) -> Option<&str> {
        self.nodes.get(name)?.prop("Unit")
    }
    /// Selectors that address a feature's value, including selectors hidden
    /// behind register address expressions. A rollback must read and write
    /// the same selected bank even when another has since been selected.
    pub(crate) fn selectors(&self, name: &str) -> Vec<String> {
        let mut dependencies = BTreeSet::new();
        let mut pending = vec![name.to_owned()];
        let mut selectors = BTreeSet::new();
        while let Some(name) = pending.pop() {
            if !dependencies.insert(name.clone()) {
                continue;
            }
            let Some(node) = self.nodes.get(&name) else {
                continue;
            };
            for property in &node.props {
                if node_link(&property.tag) && self.nodes.contains_key(&property.text) {
                    pending.push(property.text.clone());
                    if property.tag == "pIndex" || property.text.ends_with("Selector") {
                        selectors.insert(property.text.clone());
                    }
                }
            }
        }
        for (selector, node) in &self.nodes {
            if node.props.iter().any(|property| {
                property.tag == "pSelected" && dependencies.contains(&property.text)
            }) {
                selectors.insert(selector.clone());
            }
        }
        selectors.remove(name);
        selectors.into_iter().collect()
    }
    pub(crate) fn is_writable(&self, io: &mut dyn RegisterIo, name: &str) -> bool {
        self.writable(io, name, 0).is_ok()
    }
    /// A configuration value may be temporarily locked by another setting.
    /// Include its readable value in checkpoints even while it is locked;
    /// omit genuinely read-only measurements and command nodes.
    pub(crate) fn is_configuration_value(&self, io: &mut dyn RegisterIo, name: &str) -> bool {
        fn writable(
            map: &NodeMap,
            io: &mut dyn RegisterIo,
            name: &str,
            depth: usize,
        ) -> Result<bool> {
            let node = map.node(name, depth)?;
            if ["AccessMode", "ImposedAccessMode"]
                .into_iter()
                .any(|key| node.prop(key) == Some("RO"))
                || !matches!(
                    node.kind.as_str(),
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
                )
            {
                return Ok(false);
            }
            if let Some(link) = map.resolve_value_link(io, node, depth)? {
                return writable(map, io, link, depth + 1);
            }
            Ok(node.host_value()
                || matches!(
                    node.kind.as_str(),
                    "IntReg" | "MaskedIntReg" | "FloatReg" | "StringReg"
                ))
        }
        writable(self, io, name, 0).unwrap_or(false)
    }
    /// Whether a feature can be read now, following its value links: a
    /// feature hidden by `pIsAvailable` may appear after another write,
    /// unlike one that is not implemented or not in the node map.
    pub(crate) fn availability(&self, io: &mut dyn RegisterIo, name: &str) -> Availability {
        let mut name = name;
        for depth in 0..=MAX_DEPTH {
            let Ok(node) = self.node(name, depth) else {
                return Availability::Absent;
            };
            match self.gate(io, node, depth) {
                Availability::Available => {}
                gated => return gated,
            }
            match self.resolve_value_link(io, node, depth) {
                Ok(Some(link)) => name = link,
                _ => break,
            }
        }
        Availability::Available
    }
    /// Whether an enumeration entry, by symbol or entry name, can be chosen
    /// now, is hidden by `pIsAvailable`, or does not exist.
    pub(crate) fn entry_availability(
        &self,
        io: &mut dyn RegisterIo,
        name: &str,
        symbol: &str,
    ) -> Availability {
        let Ok(node) = self.node(name, 0) else {
            return Availability::Absent;
        };
        let entry = Self::enum_entries(node).into_iter().find_map(|entry| {
            self.nodes.get(entry).filter(|en| {
                symbol == entry
                    || symbol
                        == en.prop("Symbolic").unwrap_or_else(|| {
                            entry.strip_prefix(&format!("{name}_")).unwrap_or(entry)
                        })
            })
        });
        entry.map_or(Availability::Absent, |entry| self.gate(io, entry, 1))
    }
    /// One node's `pIsImplemented` and `pIsAvailable`. A condition that
    /// cannot be evaluated counts as available, so the read reports it.
    fn gate(&self, io: &mut dyn RegisterIo, node: &Node, depth: usize) -> Availability {
        for (key, closed) in [
            ("pIsImplemented", Availability::Absent),
            ("pIsAvailable", Availability::Unavailable),
        ] {
            if let Some(link) = node.prop(key)
                && self.number(io, link, depth + 1).is_ok_and(|v| v == 0.0)
            {
                return closed;
            }
        }
        Availability::Available
    }
    /// Whether `pIsLocked` holds now on the feature or a node its value
    /// links to: it reads but cannot be written.
    pub(crate) fn is_locked(&self, io: &mut dyn RegisterIo, name: &str) -> bool {
        let mut name = name;
        for depth in 0..=MAX_DEPTH {
            let Ok(node) = self.node(name, depth) else {
                return false;
            };
            if let Some(link) = node.prop("pIsLocked")
                && self.number(io, link, depth + 1).is_ok_and(|v| v != 0.0)
            {
                return true;
            }
            match self.resolve_value_link(io, node, depth) {
                Ok(Some(link)) => name = link,
                _ => break,
            }
        }
        false
    }
    /// A feature and every node its value links can reach, without
    /// evaluating selectors.
    fn value_chain(&self, name: &str) -> BTreeSet<String> {
        let mut chain = vec![name.to_owned()];
        let mut seen = BTreeSet::new();
        while let Some(name) = chain.pop() {
            if seen.len() > 4096 || !seen.insert(name.clone()) {
                continue;
            }
            if let Some(node) = self.nodes.get(&name) {
                for property in &node.props {
                    if matches!(
                        property.tag.as_str(),
                        "pValue" | "pValueIndexed" | "pValueDefault"
                    ) {
                        chain.push(property.text.clone());
                    }
                }
            }
        }
        seen
    }
    /// Whether writing any of `others` can release a `pIsLocked` on `name`:
    /// a lock condition of it or its value links reads one of them or a
    /// node their values are stored in.
    pub(crate) fn lock_released_by(&self, name: &str, others: &[&str]) -> bool {
        let inputs = self.lock_inputs(name);
        others.iter().filter(|other| **other != name).any(|other| {
            self.value_chain(other)
                .iter()
                .any(|node| inputs.contains(node))
        })
    }
    /// Every node the `pIsLocked` conditions of a feature and its value
    /// links read, transitively: the settings that can unlock it.
    fn lock_inputs(&self, name: &str) -> BTreeSet<String> {
        let mut pending = Vec::new();
        for node in self.value_chain(name) {
            if let Some(node) = self.nodes.get(&node) {
                pending.extend(
                    node.props
                        .iter()
                        .filter(|property| property.tag == "pIsLocked")
                        .map(|property| property.text.clone()),
                );
            }
        }
        let mut inputs = BTreeSet::new();
        while let Some(name) = pending.pop() {
            if inputs.len() > 4096 || !inputs.insert(name.clone()) {
                continue;
            }
            if let Some(node) = self.nodes.get(&name) {
                for property in &node.props {
                    if node_link(&property.tag) && self.nodes.contains_key(&property.text) {
                        pending.push(property.text.clone());
                    }
                }
            }
        }
        inputs
    }
    pub(crate) fn choices(&self, io: &mut dyn RegisterIo, name: &str) -> Result<Vec<String>> {
        let node = self.node(name, 0)?;
        Ok(self.entry_names(io, name, node))
    }
    fn entry_names(&self, io: &mut dyn RegisterIo, name: &str, node: &Node) -> Vec<String> {
        Self::enum_entries(node)
            .into_iter()
            .filter_map(|e| {
                self.nodes
                    .get(e)
                    .filter(|n| self.available(io, n, 0).is_ok())
                    .map(|en| {
                        en.prop("Symbolic")
                            .unwrap_or_else(|| e.strip_prefix(&format!("{name}_")).unwrap_or(e))
                            .to_owned()
                    })
            })
            .collect()
    }
    pub(crate) fn bounds(&self, io: &mut dyn RegisterIo, name: &str) -> Result<Bounds> {
        let node = self.node(name, 0)?;
        Ok(self.range(io, node, 0))
    }
    /// The increment base of an integer alias, without rounding a target's
    /// minimum through the floating-point bounds used for display.
    #[deny(clippy::cast_precision_loss)]
    fn integer_minimum(
        &self,
        io: &mut dyn RegisterIo,
        n: &Node,
        depth: usize,
    ) -> Result<Option<i128>> {
        if integer_kind(&n.kind) {
            if let Some(link) = n.prop("pMin") {
                return exact_integer(&self.numeric_value(io, link, depth + 1)?).map(Some);
            }
            if let Some(text) = n.prop("Min") {
                return exact_integer(&integer_literal(text)?).map(Some);
            }
        }
        if n.kind != "Integer" {
            // Converters may derive their bounds by evaluating a float
            // formula. Keep their existing conservative conversion policy.
            return Ok(self
                .range(io, n, depth)
                .min
                .filter(|value| value.abs() < EXACT_INTEGER_LIMIT)
                .and_then(integral_float));
        }
        let link = self.resolve_value_link(io, n, depth)?;
        let copies = n.props.iter().filter(|p| p.tag == "pValueCopy");
        let mut minimum = None;
        for target in link.into_iter().chain(copies.map(|p| p.text.as_str())) {
            let node = self.node(target, depth + 1)?;
            if let Some(value) = self.integer_minimum(io, node, depth + 1)? {
                minimum = Some(minimum.map_or(value, |old: i128| old.max(value)));
            }
        }
        Ok(minimum)
    }
    fn range(&self, io: &mut dyn RegisterIo, n: &Node, depth: usize) -> Bounds {
        let specified = |key: &str| n.prop(key).is_some() || n.prop(&format!("p{key}")).is_some();
        if matches!(n.kind.as_str(), "Integer" | "Float") {
            // Each restriction not given falls back to the pValue target's.
            // pValueCopy targets narrow them to a range all targets accept.
            let mut inner = Bounds::default();
            if !["Min", "Max", "Inc"].into_iter().all(specified) {
                let link = self.resolve_value_link(io, n, depth).ok().flatten();
                let copies = n.props.iter().filter(|p| p.tag == "pValueCopy");
                for (i, target) in link
                    .into_iter()
                    .chain(copies.map(|p| p.text.as_str()))
                    .enumerate()
                {
                    let Ok(node) = self.node(target, depth + 1) else {
                        continue;
                    };
                    let target = self.range(io, node, depth + 1);
                    if i == 0 {
                        inner = target;
                    } else {
                        inner.min = match (inner.min, target.min) {
                            (Some(a), Some(b)) => Some(a.max(b)),
                            (a, b) => a.or(b),
                        };
                        inner.max = match (inner.max, target.max) {
                            (Some(a), Some(b)) => Some(a.min(b)),
                            (a, b) => a.or(b),
                        };
                    }
                }
            }
            let mut bound = |key, inherited| {
                if specified(key) {
                    self.property_number(io, n, key, depth).ok().flatten()
                } else {
                    inherited
                }
            };
            return Bounds {
                min: bound("Min", inner.min),
                max: bound("Max", inner.max),
                inc: bound("Inc", inner.inc),
            };
        }
        let delegated = matches!(n.kind.as_str(), "Converter" | "IntConverter")
            && !["Min", "Max", "Inc"].into_iter().any(specified);
        if !delegated {
            let mut own = |key| self.property_number(io, n, key, depth).ok().flatten();
            return Bounds {
                min: own("Min"),
                max: own("Max"),
                inc: if n.kind == "IntConverter" {
                    Some(1.0)
                } else {
                    own("Inc")
                },
            };
        }
        let inner = self
            .resolve_value_link(io, n, depth)
            .ok()
            .flatten()
            .and_then(|link| self.node(link, depth + 1).ok())
            .map(|inner| self.range(io, inner, depth + 1))
            .unwrap_or_default();
        let mut from = |v: Option<f64>| {
            let v = v?;
            if n.kind == "IntConverter" {
                let to = i64::variable(&json!(v)).ok()?;
                self.formula::<i64>(io, n, "FormulaFrom", Some(("TO", to)), depth)
                    .ok()
                    .map(|v| v as f64)
            } else {
                self.formula(io, n, "FormulaFrom", Some(("TO", v)), depth)
                    .ok()
            }
        };
        let inc = (n.kind == "IntConverter").then_some(1.0);
        // A Varying formula is not monotonic, so its endpoints say nothing
        // about its range. Increasing, Decreasing and Automatic map them.
        if n.prop("Slope") == Some("Varying") {
            return Bounds {
                inc,
                ..Bounds::default()
            };
        }
        match (from(inner.min), from(inner.max)) {
            (Some(a), Some(b)) => Bounds {
                min: Some(a.min(b)),
                max: Some(a.max(b)),
                inc,
            },
            _ => Bounds {
                inc,
                ..Bounds::default()
            },
        }
    }
    fn node(&self, name: &str, depth: usize) -> Result<&Node> {
        ensure!(
            depth <= MAX_DEPTH,
            "GenICam dependency cycle or excessive nesting at {name}"
        );
        self.nodes.get(name).ok_or_else(|| {
            Categorized::error(
                ErrorKind::FeatureNotFound,
                format!("unknown GenICam feature {name}"),
            )
        })
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
        self.access_inner(io, name, write, depth, true)
    }
    fn access_inner(
        &self,
        io: &mut dyn RegisterIo,
        name: &str,
        write: bool,
        depth: usize,
        check_lock: bool,
    ) -> Result<()> {
        let node = self.node(name, depth)?;
        self.available(io, node, depth)?;
        for key in ["AccessMode", "ImposedAccessMode"] {
            if let Some(mode) = node.prop(key) {
                ensure!(
                    matches!(mode, "RO" | "WO" | "RW"),
                    "{name} has unsupported or unavailable access mode {mode}"
                );
                if write && mode == "RO" {
                    return Err(Categorized::error(
                        ErrorKind::NotWritable,
                        format!("{name} is read-only"),
                    ));
                }
                ensure!(write || mode != "WO", "{name} is write-only");
            }
        }
        if write
            && check_lock
            && let Some(link) = node.prop("pIsLocked")
            && self.number(io, link, depth + 1)? != 0.0
        {
            let owner = self.lock_owner(name);
            let mut locked =
                Categorized::new(ErrorKind::FeatureLocked, format!("{name} is locked"))
                    .details(json!({"feature": name, "locked_by": owner}));
            if let Some(owner) = &owner {
                locked = locked.hint(format!("change {owner} first, in the same set"));
            }
            return Err(locked.into());
        }
        Ok(())
    }
    /// Exposed, visible value and command features, by name.
    pub(crate) fn feature_names(&self) -> Vec<&str> {
        let exposed = self.exposed_features();
        self.nodes
            .iter()
            .filter(|(name, n)| {
                !name.starts_with("__")
                    && (exposed.as_ref().is_none_or(|names| names.contains(*name)))
                    && FEATURE_KINDS.contains(&n.kind.as_str())
                    && n.prop("Visibility") != Some("Invisible")
            })
            .map(|(name, _)| name.as_str())
            .collect()
    }
    pub(crate) fn features(&self, io: &mut dyn RegisterIo) -> Vec<FeatureInfo> {
        self.feature_names()
            .into_iter()
            .filter_map(|name| self.feature(io, name))
            .collect()
    }
    /// One feature as `features` lists it: none when this model does not
    /// implement it.
    pub(crate) fn feature(&self, io: &mut dyn RegisterIo, name: &str) -> Option<FeatureInfo> {
        let node = self.nodes.get(name)?;
        if self.availability(io, name) == Availability::Absent {
            return None;
        }
        let result = self.get(io, name);
        let (value, error) = match result {
            Ok(v) => (Some(v), None),
            Err(e) => (None, Some(format!("{e:#}"))),
        };
        let writable = self.writable(io, name, 0).is_ok();
        let choices = self.entry_names(io, name, node);
        let bounds = self.bounds(io, name).unwrap_or_default();
        Some(FeatureInfo {
            name: name.to_owned(),
            display_name: node.prop("DisplayName").unwrap_or(name).into(),
            kind: node.kind.clone(),
            value,
            writable,
            description: node
                .prop("Description")
                .or_else(|| node.prop("ToolTip"))
                .unwrap_or("")
                .into(),
            unit: node.prop("Unit").map(str::to_owned),
            min: bounds.min,
            max: bounds.max,
            inc: bounds.inc,
            choices,
            error,
        })
    }
    /// Classification needs feature names and only a few identifying values.
    /// Avoid a second full live feature/bounds/access walk during connection.
    pub(crate) fn profile_features(&self, io: &mut dyn RegisterIo) -> Vec<FeatureInfo> {
        const IDENTITY: [&str; 7] = [
            "DeviceType",
            "CameraType",
            "SensorType",
            "DeviceScanType",
            "PixelFormat",
            "ComponentSelector",
            "Scan3dOutputMode",
        ];
        let exposed = self.exposed_features();
        self.nodes
            .iter()
            .filter(|(name, node)| {
                IDENTITY.contains(&name.as_str())
                    || (exposed.as_ref().is_none_or(|names| names.contains(*name))
                        && !name.starts_with("__")
                        && node.prop("Visibility") != Some("Invisible")
                        && matches!(
                            node.kind.as_str(),
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
                        ))
            })
            .map(|(name, node)| {
                let value = if IDENTITY.contains(&name.as_str()) {
                    self.get(io, name).ok()
                } else {
                    None
                };
                FeatureInfo {
                    name: name.clone(),
                    display_name: node.prop("DisplayName").unwrap_or(name).into(),
                    kind: node.kind.clone(),
                    value,
                    writable: false,
                    description: String::new(),
                    unit: node.prop("Unit").map(str::to_owned),
                    min: None,
                    max: None,
                    inc: None,
                    choices: Vec::new(),
                    error: None,
                }
            })
            .collect()
    }
    fn exposed_features(&self) -> Option<BTreeSet<String>> {
        if self
            .nodes
            .get("Root")
            .is_none_or(|node| node.kind != "Category")
        {
            return None;
        }
        let mut features = BTreeSet::new();
        let mut pending = vec!["Root".to_owned()];
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
                    | "Register"
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
                    "IntReg" | "MaskedIntReg" | "FloatReg" | "StringReg" | "Register"
                ),
            "{name} has no writable register"
        );
        Ok(())
    }
    pub(crate) fn get(&self, io: &mut dyn RegisterIo, name: &str) -> Result<Value> {
        self.get_inner(io, name, 0)
    }
    fn get_inner(&self, io: &mut dyn RegisterIo, name: &str, depth: usize) -> Result<Value> {
        let n = self.node(name, depth)?;
        if n.kind == "Register" {
            self.available(io, n, depth)?;
            let (_, length) = self.register(io, n, depth)?;
            // Metadata only: ordinary get/features never transfer a buffer.
            return Ok(json!({"kind":"register", "length_bytes":length}));
        }
        self.access(io, name, false, depth)?;
        match n.kind.as_str() {
            "IntReg" | "MaskedIntReg" => {
                let (target, len) = self.read_register(io, n, depth)?;
                ensure!(
                    matches!(len, 1 | 2 | 4 | 8),
                    "integer register length {len} is unsupported"
                );
                let bytes = target.read(io, len)?;
                ensure!(bytes.len() == len, "short register read");
                let mut raw = decode_integer(&bytes, n.big_endian());
                let bits = if n.kind == "MaskedIntReg" {
                    let (shift, bits) = Self::mask(n, len)?;
                    raw = (raw >> shift) & bit_mask(bits);
                    bits
                } else {
                    len * 8
                };
                if n.prop("Sign") == Some("Signed") {
                    let signed = if bits == 64 {
                        raw.cast_signed()
                    } else {
                        (raw << (64 - bits)).cast_signed() >> (64 - bits)
                    };
                    Ok(json!(signed))
                } else {
                    Ok(json!(raw))
                }
            }
            "FloatReg" => {
                let (target, len) = self.read_register(io, n, depth)?;
                let b = target.read(io, len)?;
                ensure!(b.len() == len, "short float register read");
                let v = match len {
                    4 => {
                        let bytes = b
                            .as_slice()
                            .try_into()
                            .context("short float register read")?;
                        f64::from(if n.big_endian() {
                            f32::from_be_bytes(bytes)
                        } else {
                            f32::from_le_bytes(bytes)
                        })
                    }
                    8 => {
                        let bytes = b
                            .as_slice()
                            .try_into()
                            .context("short float register read")?;
                        if n.big_endian() {
                            f64::from_be_bytes(bytes)
                        } else {
                            f64::from_le_bytes(bytes)
                        }
                    }
                    _ => bail!("float register length {len} is unsupported"),
                };
                ensure!(
                    v.is_finite(),
                    "register contains non-finite floating point value"
                );
                Ok(json!(v))
            }
            "StringReg" => {
                let (target, len) = self.read_register(io, n, depth)?;
                let b = target.read(io, len)?;
                ensure!(b.len() == len, "short string register read");
                Ok(json!(
                    std::str::from_utf8(b.split(|b| *b == 0).next().unwrap_or(&[]))
                        .context("non-UTF8 string register")?
                ))
            }
            "Integer" | "Float" | "String" | "EnumEntry" => {
                if let Some(link) = self.resolve_value_link(io, n, depth)? {
                    if n.kind == "String" {
                        self.get_inner(io, link, depth + 1)
                    } else {
                        self.numeric_value(io, link, depth + 1)
                    }
                } else if let Some(index) = self.host_entry(io, n, depth)? {
                    n.value(index)
                } else {
                    bail!("{name} has no supported pValue or Value")
                }
            }
            "Boolean" => {
                let value = self.raw_value(io, n, depth)?;
                if let Some(enabled) = value.as_bool() {
                    return Ok(json!(enabled));
                }
                let on = self
                    .property_value(io, n, "OnValue", depth)?
                    .unwrap_or_else(|| json!(1));
                let off = self
                    .property_value(io, n, "OffValue", depth)?
                    .unwrap_or_else(|| json!(0));
                ensure!(
                    !numeric_equal(&on, &off)?,
                    "{name} has identical OnValue and OffValue"
                );
                let enabled = numeric_equal(&value, &on)?;
                ensure!(
                    enabled || numeric_equal(&value, &off)?,
                    "{name} contains an undefined boolean value"
                );
                Ok(json!(enabled))
            }
            "Enumeration" => {
                let value = self.raw_value(io, n, depth)?;
                // Availability limits the choices, not the reading: a value
                // set before its entry became unavailable still decodes.
                let mut matched = None;
                for entry in Self::enum_entries(n) {
                    let en = self.node(entry, depth + 1)?;
                    if numeric_equal(&self.raw_value(io, en, depth + 1)?, &value)? {
                        if self.available(io, en, depth + 1).is_ok() {
                            matched = Some((entry, en));
                            break;
                        }
                        matched.get_or_insert((entry, en));
                    }
                }
                let (entry, en) = matched
                    .ok_or_else(|| anyhow!("{name} contains unknown enumeration value {value}"))?;
                Ok(json!(en.prop("Symbolic").unwrap_or_else(|| {
                    entry.strip_prefix(&format!("{name}_")).unwrap_or(entry)
                })))
            }
            "IntSwissKnife" => Ok(json!(self.formula::<i64>(io, n, "Formula", None, depth)?)),
            "SwissKnife" => Ok(json!(self.formula::<f64>(io, n, "Formula", None, depth)?)),
            "IntConverter" => {
                let input = i64::variable(&self.raw_value(io, n, depth)?)?;
                Ok(json!(self.formula(
                    io,
                    n,
                    "FormulaFrom",
                    Some(("TO", input)),
                    depth
                )?))
            }
            "Converter" => {
                let input = self.value_number(io, n, depth)?;
                Ok(json!(self.formula(
                    io,
                    n,
                    "FormulaFrom",
                    Some(("TO", input)),
                    depth
                )?))
            }
            "Command" => Ok(Value::Null),
            other => bail!("unsupported GenApi node type {other} ({name})"),
        }
    }
    fn value_number(&self, io: &mut dyn RegisterIo, n: &Node, depth: usize) -> Result<f64> {
        number(&self.raw_value(io, n, depth)?)
    }
    fn raw_value(&self, io: &mut dyn RegisterIo, n: &Node, depth: usize) -> Result<Value> {
        if let Some(link) = self.resolve_value_link(io, n, depth)? {
            let node = self.node(link, depth + 1)?;
            if node.kind == "Enumeration" {
                self.access(io, link, false, depth + 1)?;
                self.raw_value(io, node, depth + 1)
            } else {
                self.get_inner(io, link, depth + 1)
            }
        } else if let Some(index) = self.host_entry(io, n, depth)? {
            n.value(index)
        } else {
            bail!("node has no numeric value")
        }
    }
    fn number(&self, io: &mut dyn RegisterIo, name: &str, depth: usize) -> Result<f64> {
        number(&self.numeric_value(io, name, depth)?)
    }
    /// A node's value as a number; an enumeration yields its entry's value.
    fn numeric_value(&self, io: &mut dyn RegisterIo, name: &str, depth: usize) -> Result<Value> {
        let n = self.node(name, depth)?;
        if n.kind == "Enumeration" {
            self.access(io, name, false, depth)?;
            self.raw_value(io, n, depth)
        } else {
            self.get_inner(io, name, depth)
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
        let Some(index) = self.indexed_branch(io, n, depth)? else {
            return Ok(n.prop("pValue").or_else(|| n.prop("pValueDefault")));
        };
        let branch = &n.props[index];
        Ok(branch.tag.starts_with('p').then_some(branch.text.as_str()))
    }
    /// The position of the element holding a node's host value now: its
    /// Value, or the `ValueIndexed` or `ValueDefault` its pIndex selects.
    fn host_entry(&self, io: &mut dyn RegisterIo, n: &Node, depth: usize) -> Result<Option<usize>> {
        if let Some(index) = self.indexed_branch(io, n, depth)? {
            return Ok((!n.props[index].tag.starts_with('p')).then_some(index));
        }
        Ok(n.props.iter().position(|p| p.tag == "Value"))
    }
    /// For a value table, the position of the (p)ValueIndexed entry matching
    /// the pIndex value, else of the (p)ValueDefault. The forms may be mixed.
    #[deny(clippy::cast_precision_loss)]
    fn indexed_branch(
        &self,
        io: &mut dyn RegisterIo,
        n: &Node,
        depth: usize,
    ) -> Result<Option<usize>> {
        let indexed = |p: &Element| matches!(p.tag.as_str(), "pValueIndexed" | "ValueIndexed");
        if !n.props.iter().any(indexed) {
            return Ok(None);
        }
        let selector = n
            .prop("pIndex")
            .context("indexed value node has no pIndex")?;
        // Preserve integer values until after the range check: converting
        // 2^53 + 1 to f64 first would round it down to the accepted limit.
        const LIMIT: i128 = 1 << f64::MANTISSA_DIGITS;
        let index = exact_integer(&self.numeric_value(io, selector, depth + 1)?)
            .context("indexed value selector is not an exact integer")?;
        ensure!(
            (-LIMIT..=LIMIT).contains(&index),
            "indexed value selector is not an exact integer"
        );
        let mut selected = None;
        for (position, entry) in n.props.iter().enumerate().filter(|(_, p)| indexed(p)) {
            let candidate = integer_literal(
                entry
                    .attrs
                    .get("Index")
                    .with_context(|| format!("{} has no Index", entry.tag))?,
            )
            .and_then(|value| exact_integer(&value))
            .with_context(|| format!("{} Index is not an exact integer", entry.tag))?;
            ensure!(
                (-LIMIT..=LIMIT).contains(&candidate),
                "{} Index is not an exact integer",
                entry.tag
            );
            if candidate == index {
                ensure!(
                    selected.is_none(),
                    "duplicate indexed value branch for index {index}"
                );
                selected = Some(position);
            }
        }
        selected
            .or_else(|| {
                n.props
                    .iter()
                    .position(|p| matches!(p.tag.as_str(), "pValueDefault" | "ValueDefault"))
            })
            .map(Some)
            .ok_or_else(|| anyhow!("indexed value has no branch or default for index {index}"))
    }
    fn property_number(
        &self,
        io: &mut dyn RegisterIo,
        n: &Node,
        key: &str,
        depth: usize,
    ) -> Result<Option<f64>> {
        self.property_value(io, n, key, depth)?
            .as_ref()
            .map(number)
            .transpose()
    }
    fn property_value(
        &self,
        io: &mut dyn RegisterIo,
        n: &Node,
        key: &str,
        depth: usize,
    ) -> Result<Option<Value>> {
        if let Some(link) = n.prop(&format!("p{key}")) {
            let node = self.node(link, depth + 1)?;
            if node.kind == "Enumeration" {
                self.access(io, link, false, depth + 1)?;
                self.raw_value(io, node, depth + 1).map(Some)
            } else {
                self.get_inner(io, link, depth + 1).map(Some)
            }
        } else {
            n.prop(key).map(literal).transpose()
        }
    }
    /// Resolve one live opaque Register after bank selection, without reading its body.
    pub(crate) fn raw_register_layout(
        &self,
        io: &mut dyn RegisterIo,
        name: &str,
        write: bool,
    ) -> Result<(u64, usize)> {
        let node = self.node(name, 0)?;
        if node.kind != "Register" {
            return Err(Categorized::error(
                ErrorKind::Unsupported,
                format!("{name} is not an opaque Register feature"),
            ));
        }
        self.access(io, name, write, 0)?;
        let (address, length) = self.register(io, node, 0)?;
        if length > 65536 {
            return Err(Categorized::new(
                ErrorKind::Unsupported,
                "named register transfer exceeds the 64 KiB limit",
            )
            .details(json!({"feature":name,"bytes":length,"limit_bytes":65536}))
            .into());
        }
        Ok((address, length))
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
        self.register_layout(io, n, depth)
    }
    fn register_layout(
        &self,
        io: &mut dyn RegisterIo,
        n: &Node,
        depth: usize,
    ) -> Result<(u64, usize)> {
        let length = if let Some(p) = n.prop("pLength") {
            exact_integer(&self.numeric_value(io, p, depth + 1)?)?
        } else {
            exact_integer(&literal(
                n.prop("Length").context("register has no length")?,
            )?)?
        };
        ensure!(
            length > 0 && length <= 1024 * 1024,
            "register length is outside 1..1048576"
        );
        // Address, pAddress and embedded IntSwissKnife entries are summed;
        // pIndex adds its index times Offset, pOffset or the length.
        let mut address = 0i128;
        let mut supplied = false;
        for p in &n.props {
            let offset = match p.tag.as_str() {
                "Address" => {
                    supplied = true;
                    exact_integer(&literal(&p.text)?)?
                }
                "pAddress" | "IntSwissKnife" => {
                    supplied = true;
                    exact_integer(&self.numeric_value(io, &p.text, depth + 1)?)?
                }
                "pIndex" => {
                    let index = exact_integer(&self.numeric_value(io, &p.text, depth + 1)?)?;
                    let stride = if let Some(offset) = p.attrs.get("Offset") {
                        exact_integer(&literal(offset)?)?
                    } else if let Some(offset) = p.attrs.get("pOffset") {
                        exact_integer(&self.numeric_value(io, offset, depth + 1)?)?
                    } else {
                        length
                    };
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
        let address = u64::try_from(address).context("register address is out of range")?;
        address
            .checked_add(u64::try_from(length).context("register length exceeds u64")?)
            .context("register range overflow")?;
        Ok((
            address,
            usize::try_from(length).context("register length exceeds host address size")?,
        ))
    }
    fn mask(n: &Node, len: usize) -> Result<(usize, usize)> {
        let parse = |key| -> Result<usize> {
            usize::try_from(unsigned(&literal(
                n.prop(key)
                    .ok_or_else(|| anyhow!("masked register missing {key}"))?,
            )?)?)
            .context("register bit index exceeds host address size")
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
    fn enum_entries(n: &Node) -> Vec<&str> {
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
    pub(crate) fn set(&self, io: &mut dyn RegisterIo, name: &str, value: &str) -> Result<()> {
        let n = self.node(name, 0)?;
        let kind = if n.kind == "Float" && self.direct_integer_target(io, name, 0)? {
            "Integer"
        } else {
            &n.kind
        };
        let v = input_value(name, kind, value)?;
        self.set_inner(io, name, &v, 0, true).map_err(|error| {
            crate::types::rephrase(error, format!("cannot set {name} to {value:?}"))
        })
    }
    /// Validate without changing host values or camera registers. Temporary
    /// locks are checked by the actual write, allowing auto-to-manual takeover
    /// to validate the value before disabling camera auto settings.
    pub(crate) fn validate_set(
        &self,
        io: &mut dyn RegisterIo,
        name: &str,
        value: &str,
    ) -> Result<()> {
        let n = self.node(name, 0)?;
        let kind = if n.kind == "Float" && self.direct_integer_target(io, name, 0)? {
            "Integer"
        } else {
            &n.kind
        };
        let v = input_value(name, kind, value)?;
        self.set_inner(io, name, &v, 0, false).map_err(|error| {
            crate::types::rephrase(error, format!("cannot set {name} to {value:?}"))
        })
    }
    fn set_inner(
        &self,
        io: &mut dyn RegisterIo,
        name: &str,
        value: &Value,
        depth: usize,
        write: bool,
    ) -> Result<()> {
        self.write_value(io, name, value, depth, write)?;
        if write {
            self.check_error(io, self.node(name, depth)?, depth)?;
        }
        Ok(())
    }
    /// After a write, a node's pError enumeration must read 0; another
    /// value names the device's reason in its entry.
    fn check_error(&self, io: &mut dyn RegisterIo, n: &Node, depth: usize) -> Result<()> {
        let Some(link) = n.prop("pError") else {
            return Ok(());
        };
        let code = self.numeric_value(io, link, depth + 1)?;
        if numeric_equal(&code, &json!(0))? {
            return Ok(());
        }
        let node = self.node(link, depth + 1)?;
        for entry in Self::enum_entries(node) {
            let en = self.node(entry, depth + 2)?;
            if numeric_equal(&self.entry_value(io, en, depth + 2)?, &code)? {
                let label = en
                    .prop("DisplayName")
                    .or_else(|| en.prop("Symbolic"))
                    .unwrap_or(entry);
                match en.prop("ToolTip").or_else(|| en.prop("Description")) {
                    Some(detail) => bail!("camera reported {label}: {detail}"),
                    None => bail!("camera reported {label}"),
                }
            }
        }
        bail!("camera reported error {code} in {link}")
    }
    #[deny(clippy::cast_precision_loss)]
    fn write_value(
        &self,
        io: &mut dyn RegisterIo,
        name: &str,
        value: &Value,
        depth: usize,
        write: bool,
    ) -> Result<()> {
        self.access_inner(io, name, true, depth, write)?;
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
                let enabled = self
                    .property_value(io, n, "OnValue", depth)?
                    .unwrap_or_else(|| json!(1));
                let disabled = self
                    .property_value(io, n, "OffValue", depth)?
                    .unwrap_or_else(|| json!(0));
                ensure!(
                    !numeric_equal(&enabled, &disabled)?,
                    "{name} has identical OnValue and OffValue"
                );
                if on { enabled } else { disabled }
            }
            "Enumeration" => {
                let (en, symbol) = if let Some(symbol) = value.as_str() {
                    let entry = self.find_entry(name, n, symbol).ok_or_else(|| {
                        let choices = self.entry_names(io, name, n);
                        Categorized::new(
                            ErrorKind::InvalidChoice,
                            format!(
                                "unknown {name} choice {symbol:?}; choose one of: {}",
                                choices.join(", ")
                            ),
                        )
                        .details(json!({"feature": name, "value": symbol, "choices": choices}))
                    })?;
                    (self.node(entry, depth + 1)?, symbol.to_owned())
                } else {
                    // An integer link writes an entry's value.
                    let mut found = None;
                    for entry in Self::enum_entries(n) {
                        let en = self.node(entry, depth + 1)?;
                        if numeric_equal(&self.entry_value(io, en, depth + 1)?, value)? {
                            found = Some(en);
                            break;
                        }
                    }
                    let en =
                        found.ok_or_else(|| anyhow!("{name} has no entry with value {value}"))?;
                    (en, value.to_string())
                };
                self.available(io, en, depth + 1).with_context(|| {
                    format!("{name} choice {symbol:?} is currently unavailable")
                })?;
                self.entry_value(io, en, depth + 1)?
            }
            "IntConverter" => {
                let from = i64::try_from(exact_integer(value)?)
                    .context("integer converter input exceeds the signed 64-bit range")?;
                json!(self.formula(io, n, "FormulaTo", Some(("FROM", from)), depth)?)
            }
            "Converter" => {
                json!(self.formula(io, n, "FormulaTo", Some(("FROM", number(value)?)), depth)?)
            }
            _ => value.clone(),
        };
        if !matches!(
            n.kind.as_str(),
            "String" | "StringReg" | "Boolean" | "Enumeration"
        ) {
            let mut constraint = |key| -> Result<Option<Value>> {
                n.prop(&format!("p{key}")).map_or_else(
                    || {
                        n.prop(key)
                            .map(|text| {
                                if integer_kind(&n.kind) {
                                    integer_literal(text)
                                } else {
                                    literal(text)
                                }
                            })
                            .transpose()
                    },
                    |link| self.numeric_value(io, link, depth + 1).map(Some),
                )
            };
            let (mut min, max, inc) = (constraint("Min")?, constraint("Max")?, constraint("Inc")?);
            // An increment steps from the effective minimum, which may be
            // the pValue target's.
            if min.is_none() && inc.is_some() && matches!(n.kind.as_str(), "Integer" | "Float") {
                min = if n.kind == "Integer" {
                    self.integer_minimum(io, n, depth)?
                        .map(|value| json!(value))
                } else {
                    self.range(io, n, depth).min.map(|value| json!(value))
                };
            }
            validate_numeric(
                name,
                &n.kind,
                value,
                min.as_ref(),
                max.as_ref(),
                inc.as_ref(),
            )?;
        }
        if let Some(link) = self.resolve_value_link(io, n, depth)? {
            let translated = if n.kind == "Converter" && self.integer_target(link, depth + 1)? {
                let numeric = number(&translated)?;
                ensure!(
                    numeric.abs() <= EXACT_INTEGER_LIMIT,
                    "converter output exceeds exact integer conversion range"
                );
                json!(
                    i64::try_from(
                        integral_float(numeric.trunc())
                            .context("converter output is not an integer")?
                    )
                    .context("converter output exceeds signed integer range")?
                )
            } else {
                translated
            };
            // pValueCopy targets receive the same value, in document order.
            // Check them all first so a refused copy writes nothing.
            let copies = n.props.iter().filter(|p| p.tag == "pValueCopy");
            if write {
                for copy in copies.clone() {
                    self.set_inner(io, &copy.text, &translated, depth + 1, false)?;
                }
            }
            self.set_inner(io, link, &translated, depth + 1, write)?;
            for copy in copies {
                self.set_inner(io, &copy.text, &translated, depth + 1, write)?;
            }
            return Ok(());
        }
        if n.host_value() {
            let index = self
                .host_entry(io, n, depth)?
                .with_context(|| format!("{name} has no writable value"))?;
            let stored = if n.kind == "Integer" {
                json!(signed_integer(&translated)?)
            } else {
                translated
            };
            if write {
                n.stored.borrow_mut().insert(index, stored);
            }
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
                    Self::mask(n, len)?
                } else {
                    (0, len * 8)
                };
                let raw = if n.prop("Sign") == Some("Signed") {
                    let i = signed_integer(&translated)?;
                    if bits < 64 {
                        ensure!(
                            i >= -(1i64 << (bits - 1)) && i < (1i64 << (bits - 1)),
                            "{name} requires a signed {bits}-bit integer from {} to {}",
                            -(1i64 << (bits - 1)),
                            (1i64 << (bits - 1)) - 1
                        );
                    }
                    i.cast_unsigned() & bit_mask(bits)
                } else {
                    let u = unsigned_integer(&translated)?;
                    ensure!(
                        u <= bit_mask(bits),
                        "{name} requires an unsigned {bits}-bit integer from 0 to {}",
                        bit_mask(bits)
                    );
                    u
                };
                let raw = if n.kind == "MaskedIntReg" {
                    // A write-only register cannot be read back, so its other
                    // bits are written as zero, as GenApi implementations do.
                    let old = if ["AccessMode", "ImposedAccessMode"]
                        .into_iter()
                        .any(|key| n.prop(key) == Some("WO"))
                    {
                        0
                    } else {
                        self.access(io, name, false, depth)?;
                        let old = io.read_memory(addr, len)?;
                        ensure!(old.len() == len, "short masked register read");
                        decode_integer(&old, n.big_endian())
                    };
                    (old & !(bit_mask(bits) << shift)) | (raw << shift)
                } else {
                    raw
                };
                encode_integer(raw, len, n.big_endian())
            }
            "FloatReg" => {
                let v = number(&translated)?;
                match len {
                    4 => {
                        #[expect(
                            clippy::cast_possible_truncation,
                            reason = "float32 registers round to binary32; overflow and nonzero underflow are rejected below"
                        )]
                        let stored = v as f32;
                        ensure!(
                            stored.is_finite(),
                            "{name} exceeds the float32 range (maximum magnitude {})",
                            f32::MAX
                        );
                        ensure!(
                            v == 0.0 || stored != 0.0,
                            "{name} is too small for a nonzero float32 value; use 0 or a representable value (smallest magnitude {})",
                            f32::from_bits(1)
                        );
                        encode_integer(u64::from(stored.to_bits()), 4, n.big_endian())
                    }
                    8 => encode_integer(v.to_bits(), 8, n.big_endian()),
                    _ => bail!("unsupported float register size"),
                }
            }
            "StringReg" => {
                let s = translated.as_str().context("expected string")?;
                ensure!(!s.contains('\0'), "{name} cannot contain a NUL character");
                ensure!(
                    s.len() <= len,
                    "{name} allows at most {} UTF-8 bytes; entered {} bytes",
                    len,
                    s.len()
                );
                let mut out = vec![0; len];
                out[..s.len()].copy_from_slice(s.as_bytes());
                out
            }
            _ => bail!("{name} has no supported writable register"),
        };
        if write {
            io.write_memory(addr, &bytes)
        } else {
            Ok(())
        }
    }
    fn integer_target(&self, name: &str, depth: usize) -> Result<bool> {
        let node = self.node(name, depth)?;
        match node.kind.as_str() {
            "Integer" | "IntReg" | "MaskedIntReg" | "IntConverter" => Ok(true),
            "Float" | "Converter" => node
                .prop("pValue")
                .map_or_else(|| Ok(false), |link| self.integer_target(link, depth + 1)),
            _ => Ok(false),
        }
    }
    fn direct_integer_target(
        &self,
        io: &mut dyn RegisterIo,
        name: &str,
        depth: usize,
    ) -> Result<bool> {
        let node = self.node(name, depth)?;
        if integer_kind(&node.kind) {
            return Ok(true);
        }
        if node.kind == "Float"
            && let Some(link) = self.resolve_value_link(io, node, depth)?
        {
            return self.direct_integer_target(io, link, depth + 1);
        }
        Ok(false)
    }
    pub(crate) fn execute(&self, io: &mut dyn RegisterIo, name: &str) -> Result<()> {
        self.access(io, name, true, 0)?;
        let n = self.node(name, 0)?;
        ensure!(n.kind == "Command", "{name} is not a command");
        let v = self.command_value(io, n)?;
        if let Some(target) = n.prop("pValue") {
            self.set_inner(io, target, &v, 1, true)?;
            return self.check_error(io, n, 0);
        }
        // A floating command writes its host Value.
        let index = self
            .host_entry(io, n, 0)?
            .context("command has no pValue or Value")?;
        n.stored.borrow_mut().insert(index, v);
        Ok(())
    }
    /// Poll a command's `IsDone` after `execute`: its target reads back
    /// something other than the command value. A write-only or floating
    /// command, or a target that cannot be read, counts as done. Returns
    /// false if the command is still running at `timeout`.
    pub(crate) fn wait_done(
        &self,
        io: &mut dyn RegisterIo,
        name: &str,
        timeout: std::time::Duration,
    ) -> Result<bool> {
        let n = self.node(name, 0)?;
        ensure!(n.kind == "Command", "{name} is not a command");
        let Some(target) = n.prop("pValue") else {
            return Ok(true);
        };
        if ["AccessMode", "ImposedAccessMode"]
            .into_iter()
            .any(|key| n.prop(key) == Some("WO"))
        {
            return Ok(true);
        }
        let value = self.command_value(io, n)?;
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let running = self
                .numeric_value(io, target, 1)
                .is_ok_and(|current| numeric_equal(&current, &value).unwrap_or(false));
            if !running {
                return Ok(true);
            }
            if std::time::Instant::now() >= deadline {
                return Ok(false);
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
    fn command_value(&self, io: &mut dyn RegisterIo, n: &Node) -> Result<Value> {
        if let Some(link) = n.prop("pCommandValue") {
            self.numeric_value(io, link, 1)
        } else {
            literal(
                n.prop("CommandValue")
                    .context("command has no CommandValue")?,
            )
        }
    }
    /// A pVariable's value. The `.Min`, `.Max` and `.Inc` name extensions read
    /// the linked node's bounds, `.Entry.<Name>` the value of one of its
    /// enumeration entries, and `.Value` or no extension its value.
    fn variable(
        &self,
        io: &mut dyn RegisterIo,
        var: &str,
        link: &str,
        depth: usize,
    ) -> Result<Value> {
        let Some((_, extension)) = var.split_once('.') else {
            return self.numeric_value(io, link, depth);
        };
        let node = self.node(link, depth)?;
        match extension {
            "Value" => self.numeric_value(io, link, depth),
            "Min" | "Max" | "Inc" => {
                if let Some(value) = self.property_value(io, node, extension, depth)? {
                    return Ok(value);
                }
                let bounds = self.range(io, node, depth);
                match extension {
                    "Min" => bounds.min,
                    "Max" => bounds.max,
                    _ => bounds.inc,
                }
                .map(|v| json!(v))
                .with_context(|| format!("{link} has no {extension}"))
            }
            _ => {
                let symbol = extension
                    .strip_prefix("Entry.")
                    .with_context(|| format!("unsupported formula variable {var}"))?;
                ensure!(
                    node.kind == "Enumeration",
                    "formula variable {var} needs an enumeration"
                );
                let entry = self
                    .find_entry(link, node, symbol)
                    .with_context(|| format!("{link} has no entry {symbol}"))?;
                self.entry_value(io, self.node(entry, depth + 1)?, depth + 1)
            }
        }
    }
    fn find_entry<'a>(&'a self, name: &str, n: &'a Node, symbol: &str) -> Option<&'a str> {
        Self::enum_entries(n).into_iter().find(|entry| {
            self.nodes.get(*entry).is_some_and(|en| {
                symbol == *entry
                    || symbol
                        == en.prop("Symbolic").unwrap_or_else(|| {
                            entry.strip_prefix(&format!("{name}_")).unwrap_or(entry)
                        })
            })
        })
    }
    fn entry_value(&self, io: &mut dyn RegisterIo, en: &Node, depth: usize) -> Result<Value> {
        if let Some(link) = en.prop("pValue") {
            self.get_inner(io, link, depth)
        } else {
            literal(en.prop("Value").context("enumeration entry has no value")?)
        }
    }
    /// Evaluate one of the node's formulas: as `f64` for `SwissKnife` and
    /// Converter nodes, as `i64` for `IntSwissKnife` and `IntConverter` nodes.
    fn formula<T: Operand>(
        &self,
        io: &mut dyn RegisterIo,
        n: &Node,
        key: &str,
        special: Option<(&str, T)>,
        depth: usize,
    ) -> Result<T> {
        let mut vars = BTreeMap::new();
        let mut expressions = BTreeMap::new();
        for p in &n.props {
            if matches!(p.tag.as_str(), "pVariable" | "Constant" | "Expression") {
                let name = p
                    .attrs
                    .get("Name")
                    .context("formula variable missing Name")?;
                match p.tag.as_str() {
                    "pVariable" => {
                        let value = self.variable(io, name, &p.text, depth + 1)?;
                        vars.insert(name.clone(), T::variable(&value)?);
                    }
                    "Constant" => {
                        vars.insert(name.clone(), T::literal(&literal(&p.text)?)?);
                    }
                    _ => {
                        expressions.insert(name.clone(), p.text.clone());
                    }
                }
            }
        }
        if let Some((name, value)) = special {
            vars.insert(name.into(), value);
        }
        evaluate_with(
            n.prop(key).ok_or_else(|| anyhow!("node missing {key}"))?,
            &vars,
            &expressions,
        )
    }
}
/// Whether an element names another node: a `p` link, or a register's
/// embedded address `IntSwissKnife`.
fn node_link(tag: &str) -> bool {
    tag.starts_with('p') || tag == "IntSwissKnife"
}
fn fields_has(fields: &[Element], tag: &str) -> bool {
    fields.iter().any(|p| p.tag == tag)
}
const fn bit_mask(bits: usize) -> u64 {
    if bits == 64 {
        u64::MAX
    } else {
        (1u64 << bits) - 1
    }
}
fn decode_integer(b: &[u8], big: bool) -> u64 {
    if big {
        b.iter().fold(0, |v, b| (v << 8) | u64::from(*b))
    } else {
        b.iter()
            .enumerate()
            .fold(0, |v, (i, b)| v | (u64::from(*b) << (i * 8)))
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
fn integer_kind(kind: &str) -> bool {
    matches!(kind, "Integer" | "IntReg" | "MaskedIntReg" | "IntConverter")
}

fn numeric_equal(a: &Value, b: &Value) -> Result<bool> {
    let integer = |value: &Value| {
        value
            .as_i64()
            .map(i128::from)
            .or_else(|| value.as_u64().map(i128::from))
    };
    match (integer(a), integer(b)) {
        (Some(a), Some(b)) => Ok(a == b),
        (Some(integer), None) | (None, Some(integer)) => {
            let value = number(if a.is_i64() || a.is_u64() { b } else { a })?;
            Ok(integral_float(value).is_some_and(|value| value == integer))
        }
        (None, None) => Ok(number(a)? == number(b)?),
    }
}
fn input_value(name: &str, kind: &str, text: &str) -> Result<Value> {
    match kind {
        "String" | "StringReg" | "Enumeration" => Ok(json!(text)),
        "Boolean" => {
            let text = text.trim();
            [
                ("true", true),
                ("1", true),
                ("on", true),
                ("false", false),
                ("0", false),
                ("off", false),
            ]
            .into_iter()
            .find_map(|(word, value)| text.eq_ignore_ascii_case(word).then_some(json!(value)))
            .ok_or_else(|| {
                Usage(format!(
                    "{name} must be true or false (also accepts 1/0 and on/off)"
                ))
                .into()
            })
        }
        "Command" => Err(Usage(format!("use exec for command {name}")).into()),
        "Register" => Err(Categorized::new(
            ErrorKind::Unsupported,
            format!("{name} is an opaque byte register, not a scalar setting"),
        )
        .hint("use debug read or debug write with --feature for explicit hexadecimal bytes")
        .into()),
        _ if integer_kind(kind) => integer_literal(text)
            .map_err(|error| Usage(format!("{name} requires an exact integer: {error:#}")).into()),
        _ => literal(text)
            .map_err(|error| Usage(format!("{name} requires a finite number: {error:#}")).into()),
    }
}
/// Parse integral decimal, scientific and hexadecimal forms without first
/// rounding through f64. The allowed range covers signed and unsigned registers.
fn integer_literal(text: &str) -> Result<Value> {
    let text = text.trim();
    let (negative, body) = text.strip_prefix('-').map_or_else(
        || (false, text.strip_prefix('+').unwrap_or(text)),
        |body| (true, body),
    );
    let magnitude = if let Some(hex) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16).context("invalid hexadecimal integer or integer overflow")?
    } else {
        let mut parts = body.split(['e', 'E']);
        let mantissa = parts.next().unwrap_or("");
        let exponent = parts
            .next()
            .map(str::parse::<i32>)
            .transpose()
            .context("invalid integer exponent")?
            .unwrap_or(0);
        ensure!(parts.next().is_none(), "invalid integer exponent");
        let mut decimals = mantissa.split('.');
        let whole = decimals.next().unwrap_or("");
        let fraction = decimals.next().unwrap_or("");
        ensure!(
            decimals.next().is_none()
                && !(whole.is_empty() && fraction.is_empty())
                && whole
                    .bytes()
                    .chain(fraction.bytes())
                    .all(|b| b.is_ascii_digit()),
            "expected a decimal or hexadecimal integer"
        );
        let significant_whole = whole.trim_start_matches('0');
        let significant_fraction = if significant_whole.is_empty() {
            fraction.trim_start_matches('0')
        } else {
            fraction
        };
        let mut digits =
            String::with_capacity(significant_whole.len() + significant_fraction.len());
        digits.push_str(significant_whole);
        digits.push_str(significant_fraction);
        if digits.is_empty() {
            return Ok(json!(0));
        }
        let fractional_digits =
            i64::try_from(fraction.len()).context("integer exponent overflow")?;
        let shift = i64::from(exponent)
            .checked_sub(fractional_digits)
            .context("integer exponent overflow")?;
        if shift < 0 {
            let remove =
                usize::try_from(shift.unsigned_abs()).context("integer exponent overflow")?;
            ensure!(
                remove <= digits.len() && digits.bytes().rev().take(remove).all(|b| b == b'0'),
                "fractional values are not allowed"
            );
            digits.truncate(digits.len() - remove);
        } else {
            let shift = usize::try_from(shift).context("integer overflow")?;
            ensure!(
                shift <= 20 && digits.len() <= 20 - shift,
                "integer overflow"
            );
            digits.extend(std::iter::repeat_n('0', shift));
        }
        digits.parse::<u64>().context("integer overflow")?
    };
    if negative {
        let value = -i128::from(magnitude);
        Ok(json!(
            i64::try_from(value).context("signed integer overflow")?
        ))
    } else {
        Ok(json!(magnitude))
    }
}
#[deny(clippy::cast_precision_loss)]
fn exact_integer(value: &Value) -> Result<i128> {
    if let Some(value) = value.as_i64() {
        return Ok(i128::from(value));
    }
    if let Some(value) = value.as_u64() {
        return Ok(i128::from(value));
    }
    let value = value.as_f64().context("expected an integer")?;
    ensure!(
        value.abs() <= EXACT_INTEGER_LIMIT,
        "expected an exactly representable integer"
    );
    integral_float(value).context("expected an exactly representable integer")
}
fn validate_numeric(
    name: &str,
    kind: &str,
    value: &Value,
    min: Option<&Value>,
    max: Option<&Value>,
    inc: Option<&Value>,
) -> Result<()> {
    let out_of_range = |message: String, nearest: Value| -> anyhow::Error {
        Categorized::new(ErrorKind::OutOfRange, message)
            .details(json!({
                "feature": name,
                "value": value,
                "min": min,
                "max": max,
                "inc": inc,
                "nearest": nearest,
            }))
            .into()
    };
    if integer_kind(kind) {
        let value = exact_integer(value).with_context(|| format!("{name} requires an integer"))?;
        let min = min.map(exact_integer).transpose()?;
        let max = max.map(exact_integer).transpose()?;
        ensure!(
            min.zip(max).is_none_or(|(low, high)| low <= high),
            "{name} has invalid camera bounds"
        );
        if let Some(min) = min
            && value < min
        {
            return Err(out_of_range(
                format!("{name} violates Min={min}; enter {min} or greater"),
                json!(min),
            ));
        }
        if let Some(max) = max
            && value > max
        {
            return Err(out_of_range(
                format!("{name} violates Max={max}; enter {max} or less"),
                json!(max),
            ));
        }
        if let Some(inc) = inc.map(exact_integer).transpose()? {
            ensure!(inc > 0, "{name} has invalid camera increment {inc}");
            let base = min.unwrap_or(0);
            if (value - base) % inc != 0 {
                let below = value - (value - base).rem_euclid(inc);
                let nearest = if value - below > inc / 2 && max.is_none_or(|max| below + inc <= max)
                {
                    below + inc
                } else {
                    below
                };
                return Err(out_of_range(
                    format!("{name} requires increment {inc} from {base}"),
                    json!(nearest),
                ));
            }
        }
    } else {
        let value = number(value)?;
        let min = min.map(number).transpose()?;
        let max = max.map(number).transpose()?;
        ensure!(
            min.zip(max).is_none_or(|(low, high)| low <= high),
            "{name} has invalid camera bounds"
        );
        if let Some(min) = min
            && value < min
        {
            return Err(out_of_range(
                format!("{name} violates Min={min}; enter {min} or greater"),
                json!(min),
            ));
        }
        if let Some(max) = max
            && value > max
        {
            return Err(out_of_range(
                format!("{name} violates Max={max}; enter {max} or less"),
                json!(max),
            ));
        }
        // GenApi does not verify a float increment on writes; the camera
        // rounds the value itself.
    }
    Ok(())
}
#[deny(clippy::cast_precision_loss)]
fn signed_integer(v: &Value) -> Result<i64> {
    if let Some(value) = v.as_i64() {
        return Ok(value);
    }
    let n = number(v)?;
    ensure!(
        n.abs() <= EXACT_INTEGER_LIMIT,
        "expected exact signed integer"
    );
    i64::try_from(integral_float(n).context("expected exact signed integer")?)
        .context("expected exact signed integer")
}
#[deny(clippy::cast_precision_loss)]
fn unsigned_integer(v: &Value) -> Result<u64> {
    if let Some(value) = v.as_u64() {
        return Ok(value);
    }
    let n = number(v)?;
    ensure!(
        (0.0..=EXACT_INTEGER_LIMIT).contains(&n),
        "expected exact unsigned integer"
    );
    u64::try_from(integral_float(n).context("expected exact unsigned integer")?)
        .context("expected exact unsigned integer")
}
fn unsigned(v: &Value) -> Result<u64> {
    v.as_u64()
        .ok_or_else(|| anyhow!("expected unsigned integer"))
}
fn literal(s: &str) -> Result<Value> {
    let s = s.trim();
    let body = s.strip_prefix(['+', '-']).unwrap_or(s);
    if body.starts_with("0x") || body.starts_with("0X") {
        return integer_literal(s);
    }
    if let Ok(i) = s.parse::<i64>() {
        return Ok(json!(i));
    }
    if let Ok(u) = s.parse::<u64>() {
        return Ok(json!(u));
    }
    if let Ok(integer) = integer_literal(s) {
        return Ok(integer);
    }
    let n: f64 = s.parse().context("invalid numeric value")?;
    ensure!(n.is_finite(), "expected finite number");
    ensure!(
        n != 0.0
            || !s
                .split(['e', 'E'])
                .next()
                .unwrap_or(s)
                .bytes()
                .any(|b| (b'1'..=b'9').contains(&b)),
        "numeric value is too small to represent; enter 0 explicitly or a representable nonzero value"
    );
    Ok(json!(n))
}
/// Decode a raw XML file or an in-memory ZIP, without writing device-provided paths.
#[cfg(any(feature = "usb", feature = "gentl", test))]
pub(crate) fn decode_xml(data: &[u8]) -> Result<String> {
    use std::io::{Cursor, Read as _};
    ensure!(data.len() <= MAX_XML, "GenICam file exceeds 32 MiB limit");
    let bytes = if data.starts_with(b"PK\x03\x04") {
        let mut archive = zip::ZipArchive::new(Cursor::new(data)).context("invalid GenICam ZIP")?;
        // Like the reference implementation, take the first file, preferring
        // one named *.xml when the archive also holds other files.
        let mut selected = None;
        for index in 0..archive.len() {
            let Ok(file) = archive.by_index(index) else {
                continue;
            };
            if file.is_dir() {
                continue;
            }
            selected.get_or_insert(index);
            if file
                .name()
                .rsplit_once('.')
                .is_some_and(|(_, suffix)| suffix.eq_ignore_ascii_case("xml"))
            {
                selected = Some(index);
                break;
            }
        }
        let index = selected.context("GenICam ZIP contains no file")?;
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
    let mut xml = String::from_utf8(bytes).context("GenICam XML is not UTF-8")?;
    let prefix = xml.len() - xml.trim_start_matches('\u{feff}').len();
    if prefix != 0 {
        xml.replace_range(..prefix, "");
    }
    xml.truncate(xml.trim_end_matches('\0').len());
    Ok(xml)
}

#[derive(Debug, Clone, PartialEq)]
enum Token<'a> {
    Num(Value),
    Name(&'a str),
    Op(&'static str),
    L,
    R,
    Q,
    Colon,
    Comma,
    End,
}
/// Formula arithmetic. `GenApi` evaluates `SwissKnife` and Converter formulas in
/// `f64`, and `IntSwissKnife` and `IntConverter` formulas in 64-bit integers.
trait Operand: Copy + Default {
    const INTEGER: bool;
    fn literal(value: &Value) -> Result<Self>;
    fn variable(value: &Value) -> Result<Self>;
    fn truth(self) -> bool;
    fn finite(self) -> bool;
    fn boolean(value: bool) -> Self;
    fn constant(name: &str) -> Option<Self>;
    fn unary(op: &str, value: Self) -> Result<Self>;
    fn binary(op: &str, lhs: Self, rhs: Self) -> Result<Self>;
    fn call(name: &str, args: &[Self]) -> Result<Self>;
}
impl Operand for f64 {
    const INTEGER: bool = false;
    fn literal(value: &Value) -> Result<Self> {
        number(value)
    }
    fn variable(value: &Value) -> Result<Self> {
        number(value)
    }
    fn truth(self) -> bool {
        self != 0.0
    }
    fn finite(self) -> bool {
        self.is_finite()
    }
    fn boolean(value: bool) -> Self {
        Self::from(u8::from(value))
    }
    fn constant(name: &str) -> Option<Self> {
        match name {
            "PI" => Some(std::f64::consts::PI),
            "E" => Some(std::f64::consts::E),
            _ => None,
        }
    }
    fn unary(op: &str, value: Self) -> Result<Self> {
        Ok(match op {
            "+" => value,
            "-" => -value,
            "!" => Self::boolean(value == 0.0),
            "~" => (!bits(value)?) as Self,
            _ => bail!("unsupported formula unary operator {op}"),
        })
    }
    fn binary(op: &str, lhs: Self, rhs: Self) -> Result<Self> {
        let value = match op {
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
            "&" | "|" | "^" | "<<" | ">>" => i64::binary(op, bits(lhs)?, bits(rhs)?)? as Self,
            _ => Self::boolean(comparison(op, lhs, rhs)?),
        };
        ensure!(value.is_finite(), "non-finite formula result");
        Ok(value)
    }
    fn call(name: &str, args: &[Self]) -> Result<Self> {
        let x = args[0];
        Ok(match name {
            "SGN" => Self::boolean(x > 0.0) - Self::boolean(x < 0.0),
            "NEG" => -x,
            "ABS" => x.abs(),
            "EXP" => x.exp(),
            "LN" => x.ln(),
            "LG" => x.log10(),
            "SQRT" => x.sqrt(),
            "SIN" => x.sin(),
            "COS" => x.cos(),
            "TAN" => x.tan(),
            "ASIN" => x.asin(),
            "ACOS" => x.acos(),
            "ATAN" => x.atan(),
            "FLOOR" => x.floor(),
            "CEIL" => x.ceil(),
            "TRUNC" => x.trunc(),
            "ROUND" => match args.get(1) {
                Some(&precision) => {
                    let digits =
                        i32::try_from(bits(precision)?).context("invalid ROUND precision")?;
                    let scale = 10f64.powi(digits);
                    (x * scale).round() / scale
                }
                None => x.round(),
            },
            _ => bail!("unsupported formula function {name}"),
        })
    }
}
/// Integer formulas wrap on overflow like the C `int64_t` arithmetic of
/// `GenApi` implementations, and divide with truncation toward zero.
impl Operand for i64 {
    const INTEGER: bool = true;
    fn literal(value: &Value) -> Result<Self> {
        if value.is_f64() {
            ensure!(
                number(value)?.fract() == 0.0,
                "integer formula cannot use fractional literal {value}"
            );
        }
        Self::variable(value)
    }
    /// Unsigned 64-bit registers keep their bit pattern, and Float values round
    /// to the nearest integer, as `GenApi`'s integer references do.
    #[deny(clippy::cast_precision_loss)]
    fn variable(value: &Value) -> Result<Self> {
        if let Some(v) = value.as_i64() {
            return Ok(v);
        }
        if let Some(v) = value.as_u64() {
            return Ok(v.cast_signed());
        }
        let v = number(value)?.round();
        integral_float(v)
            .and_then(|integer| Self::try_from(integer).ok())
            .with_context(|| format!("integer formula value {v} exceeds the 64-bit range"))
    }
    fn truth(self) -> bool {
        self != 0
    }
    fn finite(self) -> bool {
        true
    }
    fn boolean(value: bool) -> Self {
        value.into()
    }
    fn constant(_: &str) -> Option<Self> {
        None
    }
    fn unary(op: &str, value: Self) -> Result<Self> {
        Ok(match op {
            "+" => value,
            "-" => value.wrapping_neg(),
            "!" => Self::boolean(value == 0),
            "~" => !value,
            _ => bail!("unsupported formula unary operator {op}"),
        })
    }
    fn binary(op: &str, lhs: Self, rhs: Self) -> Result<Self> {
        Ok(match op {
            "+" => lhs.wrapping_add(rhs),
            "-" => lhs.wrapping_sub(rhs),
            "*" => lhs.wrapping_mul(rhs),
            "/" => {
                ensure!(rhs != 0, "formula division by zero");
                lhs.wrapping_div(rhs)
            }
            "%" => {
                ensure!(rhs != 0, "formula modulo by zero");
                lhs.wrapping_rem(rhs)
            }
            "**" => u32::try_from(rhs)
                .ok()
                .and_then(|exponent| lhs.checked_pow(exponent))
                .context("integer formula power overflows or has a negative exponent")?,
            "&" => lhs & rhs,
            "|" => lhs | rhs,
            "^" => lhs ^ rhs,
            "<<" | ">>" => {
                ensure!((0..64).contains(&rhs), "invalid formula shift");
                if op == "<<" { lhs << rhs } else { lhs >> rhs }
            }
            _ => Self::boolean(comparison(op, lhs, rhs)?),
        })
    }
    fn call(name: &str, args: &[Self]) -> Result<Self> {
        let x = args[0];
        Ok(match name {
            "SGN" => x.signum(),
            "NEG" => x.wrapping_neg(),
            "ABS" => x.wrapping_abs(),
            _ => bail!("unsupported formula function {name}"),
        })
    }
}
/// The standard formula functions and how many arguments each takes. Only SGN
/// and NEG are standard in integer formulas; ABS is also accepted there
/// because vendor XMLs use it in `IntSwissKnife` nodes.
fn function(name: &str, integer: bool) -> Result<std::ops::RangeInclusive<usize>> {
    let arity = match name {
        "SGN" | "NEG" | "ABS" => return Ok(1..=1),
        "ROUND" => 1..=2,
        "EXP" | "LN" | "LG" | "SQRT" | "SIN" | "COS" | "TAN" | "ASIN" | "ACOS" | "ATAN"
        | "FLOOR" | "CEIL" | "TRUNC" => 1..=1,
        _ => bail!("unsupported formula function {name}"),
    };
    ensure!(
        !integer,
        "formula function {name} is not available in integer formulas"
    );
    Ok(arity)
}
fn comparison<T: PartialOrd + Copy>(op: &str, lhs: T, rhs: T) -> Result<bool> {
    Ok(match op {
        "==" | "=" => lhs == rhs,
        "!=" | "<>" => lhs != rhs,
        "<" => lhs < rhs,
        ">" => lhs > rhs,
        "<=" => lhs <= rhs,
        ">=" => lhs >= rhs,
        _ => bail!("unsupported formula binary operator {op}"),
    })
}
fn evaluate<T: Operand>(expression: &str, vars: &BTreeMap<String, T>) -> Result<T> {
    evaluate_with(expression, vars, &BTreeMap::new())
}
/// Named `Expression` sub-formulas are evaluated where the formula uses them,
/// with every variable bound, including a converter's TO or FROM. As the
/// standard requires, they cannot refer to other sub-formulas.
fn evaluate_with<T: Operand>(
    expression: &str,
    vars: &BTreeMap<String, T>,
    expressions: &BTreeMap<String, String>,
) -> Result<T> {
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
            tokens.push(Token::Num(literal(
                expression
                    .get(start..i)
                    .context("invalid formula literal boundary")?,
            )?));
            continue;
        }
        if bytes[i].is_ascii_alphabetic() || bytes[i] == b'_' {
            i += 1;
            while i < bytes.len()
                && (bytes[i].is_ascii_alphanumeric()
                    || bytes[i] == b'_'
                    || bytes[i] == b'.' && bytes.get(i + 1).is_some_and(u8::is_ascii_alphabetic))
            {
                i += 1;
            }
            tokens.push(Token::Name(
                expression
                    .get(start..i)
                    .context("invalid formula name boundary")?,
            ));
            continue;
        }
        let double = match (bytes[i], bytes.get(i + 1).copied()) {
            (b'<', Some(b'<')) => Some("<<"),
            (b'>', Some(b'>')) => Some(">>"),
            (b'<', Some(b'=')) => Some("<="),
            (b'>', Some(b'=')) => Some(">="),
            (b'=', Some(b'=')) => Some("=="),
            (b'!', Some(b'=')) => Some("!="),
            (b'<', Some(b'>')) => Some("<>"),
            (b'&', Some(b'&')) => Some("&&"),
            (b'|', Some(b'|')) => Some("||"),
            (b'*', Some(b'*')) => Some("**"),
            _ => None,
        };
        if let Some(op) = double {
            tokens.push(Token::Op(op));
            i += 2;
            continue;
        }
        tokens.push(match bytes[i] {
            b'(' => Token::L,
            b')' => Token::R,
            b'?' => Token::Q,
            b':' => Token::Colon,
            b',' => Token::Comma,
            b'+' => Token::Op("+"),
            b'-' => Token::Op("-"),
            b'*' => Token::Op("*"),
            b'/' => Token::Op("/"),
            b'%' => Token::Op("%"),
            b'&' => Token::Op("&"),
            b'|' => Token::Op("|"),
            b'^' => Token::Op("^"),
            b'~' => Token::Op("~"),
            b'!' => Token::Op("!"),
            b'<' => Token::Op("<"),
            b'>' => Token::Op(">"),
            b'=' => Token::Op("="),
            _ => bail!("unsupported formula syntax at byte {i}"),
        });
        i += 1;
    }
    tokens.push(Token::End);
    let mut parser = Expr {
        tokens,
        pos: 0,
        vars,
        expressions,
        depth: 0,
        skip: 0,
    };
    let value = parser.expr(0)?;
    ensure!(
        parser.tokens[parser.pos] == Token::End,
        "unsupported trailing formula syntax"
    );
    ensure!(value.finite(), "formula produced non-finite result");
    Ok(value)
}
struct Expr<'a, T> {
    tokens: Vec<Token<'a>>,
    pos: usize,
    vars: &'a BTreeMap<String, T>,
    expressions: &'a BTreeMap<String, String>,
    depth: usize,
    /// Nonzero while parsing an operand whose value is discarded: the untaken
    /// branch of `?:` or the short-circuited side of `&&` and `||`. Its syntax
    /// is still checked, but its arithmetic cannot fail the formula.
    skip: usize,
}
impl<T: Operand> Expr<'_, T> {
    fn compute(&self, value: impl FnOnce() -> Result<T>) -> Result<T> {
        if self.skip > 0 {
            Ok(T::default())
        } else {
            value()
        }
    }
    fn discard(&mut self, skip: bool, min: u8) -> Result<T> {
        self.skip += usize::from(skip);
        let value = self.expr(min);
        self.skip -= usize::from(skip);
        value
    }
    fn operand(&mut self) -> Result<T> {
        let token = self.tokens[self.pos].clone();
        self.pos += 1;
        Ok(match token {
            Token::Num(n) => T::literal(&n)?,
            Token::Name(n) => {
                if self.tokens[self.pos] == Token::L {
                    self.pos += 1;
                    let name = n.to_ascii_uppercase();
                    let arity = function(&name, T::INTEGER)?;
                    let mut args = [T::default(); 2];
                    args[0] = self.expr(0)?;
                    let mut count = 1;
                    while self.tokens[self.pos] == Token::Comma {
                        self.pos += 1;
                        let value = self.expr(0)?;
                        if let Some(argument) = args.get_mut(count) {
                            *argument = value;
                        }
                        count += 1;
                    }
                    ensure!(
                        self.tokens[self.pos] == Token::R,
                        "unclosed formula function"
                    );
                    self.pos += 1;
                    ensure!(
                        count <= args.len() && arity.contains(&count),
                        "wrong number of arguments for formula function {name}"
                    );
                    self.compute(|| T::call(&name, &args[..count]))?
                } else if let Some(&value) = self.vars.get(n) {
                    value
                } else if let Some(text) = self.expressions.get(n) {
                    self.compute(|| {
                        evaluate(text, self.vars).with_context(|| format!("formula expression {n}"))
                    })?
                } else {
                    T::constant(n).ok_or_else(|| anyhow!("unknown formula variable {n}"))?
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
            Token::Op(op) if matches!(op, "+" | "-" | "!" | "~") => {
                let v = self.expr(12)?;
                self.compute(|| T::unary(op, v))?
            }
            _ => bail!("expected formula operand"),
        })
    }
    /// Each nested sub-expression counts toward the depth limit, but the else
    /// branch of `?:` continues in the same frame, so long right-nested
    /// selector chains such as `(F=1)?(A):((F=2)?(B):(...))` stay shallow.
    fn expr(&mut self, min: u8) -> Result<T> {
        self.depth += 1;
        ensure!(
            self.depth <= MAX_FORMULA_DEPTH,
            "formula nesting exceeds limit"
        );
        let skip = self.skip;
        let mut chosen = None;
        let mut lhs = self.operand()?;
        loop {
            if self.tokens[self.pos] == Token::Q && min == 0 {
                self.pos += 1;
                let taken = self.skip == 0 && lhs.truth();
                let yes = self.discard(!taken, 0)?;
                ensure!(
                    self.tokens[self.pos] == Token::Colon,
                    "expected ':' in formula"
                );
                self.pos += 1;
                if taken {
                    chosen = Some(yes);
                    self.skip += 1;
                }
                lhs = self.operand()?;
                continue;
            }
            let Token::Op(op) = &self.tokens[self.pos] else {
                break;
            };
            let prec = match *op {
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
            let op = *op;
            self.pos += 1;
            let decided = match op {
                "&&" => !lhs.truth(),
                "||" => lhs.truth(),
                _ => false,
            };
            let rhs = self.discard(decided, if op == "**" { prec } else { prec + 1 })?;
            lhs = match op {
                "&&" | "||" => T::boolean(if decided { op == "||" } else { rhs.truth() }),
                _ => self.compute(|| T::binary(op, lhs, rhs))?,
            };
        }
        self.skip = skip;
        self.depth -= 1;
        Ok(chosen.unwrap_or(lhs))
    }
}
/// Bitwise operators truncate a floating-point operand toward zero, like the
/// C conversion to `int64_t` in `GenApi` implementations.
#[deny(clippy::cast_precision_loss)]
fn bits(v: f64) -> Result<i64> {
    integral_float(v.trunc())
        .and_then(|integer| i64::try_from(integer).ok())
        .context("bitwise formula operand outside the 64-bit integer range")
}

#[cfg(test)]
mod tests {
    #[test]
    fn unsupported_formula_operators_fail_closed_for_both_numeric_types() {
        for operator in ["?", "//", "==="] {
            for error in [
                i64::unary(operator, 3).unwrap_err(),
                f64::unary(operator, 3.0).unwrap_err(),
            ] {
                assert_eq!(
                    error.to_string(),
                    format!("unsupported formula unary operator {operator}")
                );
            }
            for error in [
                i64::binary(operator, 3, 2).unwrap_err(),
                f64::binary(operator, 3.0, 2.0).unwrap_err(),
            ] {
                assert_eq!(
                    error.to_string(),
                    format!("unsupported formula binary operator {operator}")
                );
            }
        }
    }

    use super::*;
    use std::fmt::Write as _;
    use std::io::Cursor;
    #[test]
    fn image_geometry_and_format_are_locked_while_streaming() {
        for name in [
            "Width",
            "OffsetY",
            "PixelFormat",
            "BinningVertical",
            "VideoMode",
        ] {
            assert!(stream_locked(name), "{name}");
        }
        for name in ["ExposureTime", "Gain", "AcquisitionFrameRate", "WidthMax"] {
            assert!(!stream_locked(name), "{name}");
        }
    }
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
    fn opaque_register_descriptors_never_read_bodies_or_enter_configuration() {
        struct NoBody;
        impl RegisterIo for NoBody {
            fn read_memory(&mut self, _: u64, _: usize) -> Result<Vec<u8>> {
                anyhow::bail!("unexpected buffer read")
            }
            fn write_memory(&mut self, _: u64, _: &[u8]) -> Result<()> {
                anyhow::bail!("unexpected buffer write")
            }
        }
        let m = map(
            "<Register Name='Large'><Address>0xA1000</Address><Length>1048576</Length><AccessMode>RW</AccessMode></Register><Register Name='WriteOnly'><Address>0x2000</Address><Length>8192</Length><AccessMode>WO</AccessMode></Register>",
        );
        let mut io = NoBody;
        assert!(m.feature_names().contains(&"Large"));
        assert_eq!(
            m.get(&mut io, "Large").unwrap(),
            json!({"kind":"register","length_bytes":1_048_576})
        );
        assert_eq!(
            m.get(&mut io, "WriteOnly").unwrap(),
            json!({"kind":"register","length_bytes":8192})
        );
        assert!(!m.is_configuration_value(&mut io, "Large"));
        assert!(!m.is_configuration_value(&mut io, "WriteOnly"));
        assert!(m.feature(&mut io, "Large").unwrap().writable);
        assert_eq!(
            crate::types::classify(&m.set(&mut io, "Large", "00").unwrap_err()).kind,
            ErrorKind::Unsupported
        );
        assert_eq!(
            crate::types::classify(&m.raw_register_layout(&mut io, "Large", false).unwrap_err())
                .kind,
            ErrorKind::Unsupported
        );
        assert!(
            format!(
                "{:#}",
                m.raw_register_layout(&mut io, "WriteOnly", false)
                    .unwrap_err()
            )
            .contains("write-only")
        );
    }
    #[test]
    fn explicit_raw_register_layout_rejects_access_ports_lengths_and_overflow() {
        let mut io = Memory::default();
        for (body, write, message) in [
            (
                "<Register Name='R'><Address>0</Address><Length>8</Length><AccessMode>RO</AccessMode></Register>",
                true,
                "read-only",
            ),
            (
                "<Register Name='R'><Address>0</Address><Length>8</Length><pIsLocked>Locked</pIsLocked></Register><Integer Name='Locked'><Value>1</Value></Integer>",
                true,
                "locked",
            ),
            (
                "<Register Name='R'><Address>0</Address><Length>8</Length><pPort>P</pPort></Register><Port Name='P'><ChunkID>1</ChunkID></Port>",
                false,
                "chunk and event",
            ),
            (
                "<Register Name='R'><Address>0</Address><Length>8</Length><pPort>P</pPort></Register><Port Name='P'><EventID>1</EventID></Port>",
                false,
                "chunk and event",
            ),
            (
                "<Register Name='R'><Address>0</Address><Length>0</Length></Register>",
                false,
                "length",
            ),
            (
                "<Register Name='R'><Address>18446744073709551615</Address><Length>2</Length></Register>",
                false,
                "overflow",
            ),
        ] {
            let error = map(body)
                .raw_register_layout(&mut io, "R", write)
                .unwrap_err();
            assert!(format!("{error:#}").contains(message), "{error:#}");
        }
        assert_eq!(
            map("<Register Name='R'><Address>16</Address><Length>65536</Length></Register>")
                .raw_register_layout(&mut io, "R", false)
                .unwrap(),
            (16, 65536)
        );
        assert!(io.bytes.is_empty());
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
    fn index_stride_defaults_to_the_register_length_and_reads_p_offset() {
        let m = map(concat!(
            "<IntReg Name='Indexed'><Address>100</Address><Address>20</Address>",
            "<pIndex Offset='1000'>Index1</pIndex><pIndex Offset='10000'>Index2</pIndex>",
            "<pIndex>Index3</pIndex><Length>4</Length></IntReg>",
            "<Integer Name='Index1'><Value>5</Value></Integer>",
            "<Integer Name='Index2'><Value>3</Value></Integer>",
            "<Integer Name='Index3'><Value>2</Value></Integer>",
            "<IntReg Name='Gain'><Address>0x1000</Address><pIndex pOffset='Stride'>GainSelector</pIndex><Length>4</Length></IntReg>",
            "<Integer Name='Stride'><Value>0x10</Value></Integer>",
            "<Enumeration Name='GainSelector'><EnumEntry Name='All'><Value>0</Value></EnumEntry><EnumEntry Name='Red'><Value>3</Value></EnumEntry><Value>3</Value></Enumeration>",
        ));
        let mut io = Memory::default();
        let indexed = m.node("Indexed", 0).unwrap();
        assert_eq!(m.register(&mut io, indexed, 0).unwrap(), (35128, 4));
        m.set(&mut io, "Gain", "7").unwrap();
        assert_eq!(io.read_memory(0x1030, 4).unwrap(), 7u32.to_le_bytes());
    }
    #[test]
    fn embedded_swiss_knives_add_to_register_addresses() {
        // Shaped like Allied Vision Alvium per-line and inquiry registers.
        let m = map(concat!(
            "<Enumeration Name='LineSelector'><EnumEntry Name='Line0'><Value>0</Value></EnumEntry><EnumEntry Name='Line1'><Value>1</Value></EnumEntry><Value>0</Value></Enumeration>",
            "<IntSwissKnife Name='LineSelectorIndex'><pVariable Name='LINESELECTOR'>LineSelector</pVariable><Formula>LINESELECTOR &amp; 0xFF</Formula></IntSwissKnife>",
            "<Enumeration Name='LineMode'><EnumEntry Name='Input'><Value>0</Value></EnumEntry><EnumEntry Name='Output'><Value>1</Value></EnumEntry><pValue>RegLineMode</pValue></Enumeration>",
            "<IntReg Name='RegLineMode'><Address>0x10380020</Address><IntSwissKnife Name='RegLineMode_Offset'><pVariable Name='LINESELECTORINDEX'>LineSelectorIndex</pVariable><Formula>LINESELECTORINDEX * 0x04</Formula></IntSwissKnife><Length>0x04</Length><AccessMode>RW</AccessMode><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess></IntReg>",
            "<StructReg Comment='StructDeviceVendorNameInq'><Address>0x00800000</Address><IntSwissKnife Name='StructDeviceVendorNameImpl_Offset'><Formula>1 * 0x04</Formula></IntSwissKnife><Length>0x04</Length><AccessMode>RO</AccessMode><Endianess>LittleEndian</Endianess><StructEntry Name='RegDeviceVendorNameImpl'><Bit>0</Bit></StructEntry><StructEntry Name='RegDeviceVendorNameAvail'><Bit>1</Bit></StructEntry></StructReg>",
            "<IntReg Name='DeviceTemperatureRegister'><IntSwissKnife><Formula>0x2000 + 0x10</Formula></IntSwissKnife><Length>4</Length><AccessMode>RO</AccessMode></IntReg>",
        ));
        let mut io = Memory::default();
        m.set(&mut io, "LineSelector", "Line1").unwrap();
        m.set(&mut io, "LineMode", "Output").unwrap();
        assert_eq!(io.read_memory(0x1038_0024, 4).unwrap(), [1, 0, 0, 0]);
        assert_eq!(io.read_memory(0x1038_0020, 4).unwrap(), [0; 4]);
        assert_eq!(m.selectors("LineMode"), ["LineSelector"]);
        assert_eq!(m.get(&mut io, "RegLineMode_Offset").unwrap(), 4);
        io.write_memory(0x0080_0004, &[2, 0, 0, 0]).unwrap();
        assert_eq!(m.get(&mut io, "RegDeviceVendorNameImpl").unwrap(), 0);
        assert_eq!(m.get(&mut io, "RegDeviceVendorNameAvail").unwrap(), 1);
        io.write_memory(0x2010, &[9, 0, 0, 0]).unwrap();
        assert_eq!(m.get(&mut io, "DeviceTemperatureRegister").unwrap(), 9);
        assert!(
            m.features(&mut io)
                .iter()
                .all(|feature| !feature.name.starts_with("__"))
        );
    }
    #[test]
    fn integer_links_read_enumeration_values_and_boolean_values_accept_words() {
        let m = map(concat!(
            "<Enumeration Name='Bank'><EnumEntry Name='Low'><Value>0x100</Value></EnumEntry><EnumEntry Name='High'><Value>0x200</Value></EnumEntry><Value>0x200</Value></Enumeration>",
            "<IntReg Name='Banked'><pAddress>Bank</pAddress><Length>4</Length></IntReg>",
            "<Integer Name='BankValue'><pValue>Bank</pValue></Integer>",
            "<Boolean Name='Floating'><Value>true</Value></Boolean>",
        ));
        let mut io = Memory::default();
        io.write_memory(0x200, &[5, 0, 0, 0]).unwrap();
        assert_eq!(m.get(&mut io, "Banked").unwrap(), 5);
        assert_eq!(m.get(&mut io, "BankValue").unwrap(), 0x200);
        m.set(&mut io, "BankValue", "256").unwrap();
        assert_eq!(m.get(&mut io, "Bank").unwrap(), "Low");
        assert!(m.set(&mut io, "BankValue", "3").is_err());
        assert_eq!(m.get(&mut io, "Floating").unwrap(), true);
        m.set(&mut io, "Floating", "false").unwrap();
        assert_eq!(m.get(&mut io, "Floating").unwrap(), false);
    }
    #[test]
    fn enumeration_reads_a_current_entry_that_became_unavailable() {
        let m = map(concat!(
            "<Enumeration Name='PixelFormat'><EnumEntry Name='Mono8'><Value>0x01080001</Value></EnumEntry>",
            "<EnumEntry Name='Mono12'><Value>0x01100005</Value><pIsAvailable>Deep</pIsAvailable></EnumEntry>",
            "<pValue>Format</pValue></Enumeration>",
            "<Integer Name='Format'><Value>0x01100005</Value></Integer>",
            "<Integer Name='Deep'><Value>0</Value></Integer>",
        ));
        let mut io = Memory::default();
        assert_eq!(m.get(&mut io, "PixelFormat").unwrap(), "Mono12");
        assert_eq!(m.choices(&mut io, "PixelFormat").unwrap(), ["Mono8"]);
        assert!(m.set(&mut io, "PixelFormat", "Mono12").is_err());
    }
    #[test]
    fn value_copies_are_written_and_bounded_and_errors_are_reported() {
        let m = map(concat!(
            "<Integer Name='Gain'><pValue>GainReg</pValue><pValueCopy>GainShadow</pValueCopy><pError>Status</pError></Integer>",
            "<IntReg Name='GainReg'><Address>0</Address><Length>4</Length><AccessMode>RW</AccessMode></IntReg>",
            "<Integer Name='GainShadow'><pValue>ShadowReg</pValue><Min>0</Min><Max>100</Max></Integer>",
            "<IntReg Name='ShadowReg'><Address>4</Address><Length>4</Length><AccessMode>RW</AccessMode></IntReg>",
            "<Enumeration Name='Status'><EnumEntry Name='Ok'><Value>0</Value></EnumEntry>",
            "<EnumEntry Name='OutOfRange'><DisplayName>Out of range</DisplayName><ToolTip>The gain exceeds the sensor limit.</ToolTip><Value>1</Value></EnumEntry>",
            "<pValue>StatusReg</pValue></Enumeration>",
            "<IntReg Name='StatusReg'><Address>8</Address><Length>4</Length><AccessMode>RO</AccessMode></IntReg>",
        ));
        let mut io = Memory::default();
        assert_eq!(m.bounds(&mut io, "Gain").unwrap().max, Some(100.0));
        m.set(&mut io, "Gain", "42").unwrap();
        assert_eq!(io.read_memory(0, 8).unwrap(), [42, 0, 0, 0, 42, 0, 0, 0]);
        assert!(m.set(&mut io, "Gain", "101").is_err());
        assert_eq!(io.read_memory(0, 4).unwrap(), [42, 0, 0, 0]);
        io.write_memory(8, &[1, 0, 0, 0]).unwrap();
        let error = format!("{:#}", m.set(&mut io, "Gain", "7").unwrap_err());
        assert!(
            error.contains("Out of range: The gain exceeds the sensor limit."),
            "{error}"
        );
    }
    #[test]
    fn missing_restrictions_fall_back_to_the_value_target() {
        let m = map(concat!(
            "<Integer Name='Width'><pValue>WidthInner</pValue><pMax>WidthMax</pMax><Inc>16</Inc></Integer>",
            "<Integer Name='WidthInner'><pValue>WidthReg</pValue><Min>8</Min><Max>4096</Max></Integer>",
            "<Integer Name='WidthMax'><Value>1928</Value></Integer>",
            "<IntReg Name='WidthReg'><Address>0</Address><Length>4</Length><AccessMode>RW</AccessMode></IntReg>",
        ));
        let mut io = Memory::default();
        assert_eq!(
            m.bounds(&mut io, "Width").unwrap(),
            bounds(8.0, 1928.0, Some(16.0))
        );
        m.set(&mut io, "Width", "24").unwrap();
        assert_eq!(m.get(&mut io, "Width").unwrap(), 24);
        assert!(m.set(&mut io, "Width", "32").is_err());
        assert!(m.set(&mut io, "Width", "1944").is_err());
    }
    #[test]
    fn literal_value_tables_keep_one_value_per_index() {
        let m = map(concat!(
            "<Integer Name='Table'><pIndex>TableSelector</pIndex><ValueIndexed Index='10'>100</ValueIndexed><ValueIndexed Index='20'>200</ValueIndexed><ValueDefault>600</ValueDefault></Integer>",
            "<Integer Name='TableSelector'><Value>20</Value></Integer>",
            "<Float Name='FloatTable'><pIndex>TableSelector</pIndex><ValueIndexed Index='10'>100.1</ValueIndexed><pValueIndexed Index='20'>Shared</pValueIndexed><pValueDefault>Shared</pValueDefault></Float>",
            "<Float Name='Shared'><Value>600.6</Value></Float>",
        ));
        let mut io = Memory::default();
        assert_eq!(m.get(&mut io, "Table").unwrap(), 200);
        m.set(&mut io, "Table", "150").unwrap();
        assert_eq!(m.get(&mut io, "Table").unwrap(), 150);
        m.set(&mut io, "TableSelector", "-1").unwrap();
        assert_eq!(m.get(&mut io, "Table").unwrap(), 600);
        m.set(&mut io, "TableSelector", "10").unwrap();
        assert_eq!(m.get(&mut io, "Table").unwrap(), 100);
        assert_eq!(m.get(&mut io, "FloatTable").unwrap(), 100.1);
        m.set(&mut io, "FloatTable", "1.5").unwrap();
        assert_eq!(m.get(&mut io, "Shared").unwrap(), 600.6);
        m.set(&mut io, "TableSelector", "20").unwrap();
        assert_eq!(m.get(&mut io, "Table").unwrap(), 150);
        assert_eq!(m.get(&mut io, "FloatTable").unwrap(), 600.6);
        m.set(&mut io, "TableSelector", "10").unwrap();
        assert_eq!(m.get(&mut io, "FloatTable").unwrap(), 1.5);
        assert!(m.is_writable(&mut io, "Table"));
    }
    #[test]
    fn indexed_values_validate_integers_before_float_rounding() {
        for selector in ["9007199254740993", "-9007199254740993"] {
            let m = map(&format!(
                "<Integer Name='Selector'><Value>{selector}</Value></Integer><Integer Name='Table'><pIndex>Selector</pIndex><ValueIndexed Index='9007199254740992'>1</ValueIndexed><ValueDefault>2</ValueDefault></Integer>"
            ));
            let error = m.get(&mut Memory::default(), "Table").unwrap_err();
            assert!(format!("{error:#}").contains("selector is not an exact integer"));
        }
        for index in [
            "9007199254740993",
            "-9007199254740993",
            "9007199254740992.1",
            "-9007199254740992.1",
            "9007199254740993e0",
        ] {
            let m = map(&format!(
                "<Integer Name='Selector'><Value>0</Value></Integer><Integer Name='Table'><pIndex>Selector</pIndex><ValueIndexed Index='{index}'>1</ValueIndexed><ValueDefault>2</ValueDefault></Integer>"
            ));
            let error = m.get(&mut Memory::default(), "Table").unwrap_err();
            assert!(format!("{error:#}").contains("Index is not an exact integer"));
        }
        for index in [
            "9007199254740992",
            "-9007199254740992",
            "9.007199254740992e15",
            "0x20000000000000",
        ] {
            let m = map(&format!(
                "<Integer Name='Selector'><Value>{index}</Value></Integer><Integer Name='Table'><pIndex>Selector</pIndex><ValueIndexed Index='{index}'>1</ValueIndexed><ValueDefault>2</ValueDefault></Integer>"
            ));
            assert_eq!(m.get(&mut Memory::default(), "Table").unwrap(), 1);
        }
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
        missing.get(&mut io, "Mux").unwrap_err();
    }
    #[test]
    fn float_register_reads_preserve_width_and_byte_order() {
        for (endian, big) in [("LittleEndian", false), ("BigEndian", true)] {
            for (length, bytes, expected) in [
                (
                    4,
                    if big {
                        (-2.5f32).to_be_bytes()
                    } else {
                        (-2.5f32).to_le_bytes()
                    }
                    .to_vec(),
                    -2.5,
                ),
                (
                    8,
                    if big {
                        f64::MIN_POSITIVE.to_be_bytes()
                    } else {
                        f64::MIN_POSITIVE.to_le_bytes()
                    }
                    .to_vec(),
                    f64::MIN_POSITIVE,
                ),
            ] {
                let m = map(&format!(
                    "<FloatReg Name='R'><Address>0</Address><Length>{length}</Length><Endianess>{endian}</Endianess></FloatReg>"
                ));
                let mut io = Memory::default();
                io.write_memory(0, &bytes).unwrap();
                assert_eq!(m.get(&mut io, "R").unwrap().as_f64(), Some(expected));
            }
        }
    }

    #[test]
    fn oversized_mask_indices_cannot_wrap_into_the_register() {
        for bit in [32, 1u64 << 32, (1u64 << 32) + 3, u64::MAX] {
            for mask in [
                format!("<Bit>{bit}</Bit>"),
                format!("<LSB>0</LSB><MSB>{bit}</MSB>"),
                format!("<LSB>{bit}</LSB><MSB>31</MSB>"),
            ] {
                let m = map(&format!(
                    "<MaskedIntReg Name='R'><Address>0</Address><Length>4</Length>{mask}</MaskedIntReg>"
                ));
                let mut io = Memory::default();
                io.write_memory(0, &[0xa5; 4]).unwrap();
                assert!(m.get(&mut io, "R").is_err(), "accepted {mask}");
                assert!(m.set(&mut io, "R", "1").is_err(), "accepted {mask}");
                assert_eq!(io.read_memory(0, 4).unwrap(), [0xa5; 4]);
            }
        }
    }

    #[test]
    fn exact_integer_literals_keep_their_signed_and_exponent_bounds() {
        for (text, expected) in [
            ("-9223372036854775808", json!(i64::MIN)),
            ("-9.223372036854775808e18", json!(i64::MIN)),
            ("1.8446744073709551615e19", json!(u64::MAX)),
            ("100e-2", json!(1)),
            ("1e19", json!(10_000_000_000_000_000_000u64)),
            ("000001.0200e2", json!(102)),
            ("000.0001200e5", json!(12)),
            ("0001.000e0", json!(1)),
            (".000010e5", json!(1)),
            ("000.000e-2147483648", json!(0)),
        ] {
            assert_eq!(integer_literal(text).unwrap(), expected, "{text}");
        }
        for text in [
            "-9223372036854775809",
            "1e20",
            "1e2147483647",
            "1e-2147483648",
            "000.0001200e4",
            "0001.001e0",
        ] {
            assert!(integer_literal(text).is_err(), "accepted {text}");
        }
        let zeros = "0".repeat(65_536);
        for text in [
            format!("{zeros}1.00"),
            format!("0.{zeros}1e{}", zeros.len() + 1),
        ] {
            assert_eq!(integer_literal(&text).unwrap(), 1);
        }
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
    fn write_only_masked_registers_are_written_without_reading() {
        struct WriteOnly(Memory);
        impl RegisterIo for WriteOnly {
            fn read_memory(&mut self, a: u64, _: usize) -> Result<Vec<u8>> {
                bail!("GVCP status access denied reading {a:#x}")
            }
            fn write_memory(&mut self, a: u64, b: &[u8]) -> Result<()> {
                self.0.write_memory(a, b)
            }
        }
        let m = map(concat!(
            "<Command Name='TriggerSoftware'><pValue>TriggerSoftwareBit</pValue><CommandValue>1</CommandValue></Command>",
            "<StructReg Comment='Control'><Address>0x0c</Address><Length>4</Length><AccessMode>WO</AccessMode><Endianess>BigEndian</Endianess>",
            "<StructEntry Name='TriggerSoftwareBit'><Bit>31</Bit></StructEntry><StructEntry Name='ResetBit'><Bit>0</Bit></StructEntry></StructReg>",
        ));
        let mut io = WriteOnly(Memory::default());
        m.execute(&mut io, "TriggerSoftware").unwrap();
        assert_eq!(io.0.read_memory(0x0c, 4).unwrap(), [0, 0, 0, 1]);
        m.set(&mut io, "ResetBit", "1").unwrap();
        assert_eq!(io.0.read_memory(0x0c, 4).unwrap(), [0x80, 0, 0, 0]);
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
    fn xml_decoding_preserves_text_while_removing_padding() {
        use std::io::Write as _;

        for prefix in ["", "\u{feff}", "\u{feff}\u{feff}"] {
            for body in ["", "<XML/>", " <相机/>\n", "<XML>\0</XML>"] {
                for suffix in ["", "\0", "\0\0"] {
                    let padded = format!("{prefix}{body}{suffix}");
                    assert_eq!(decode_xml(padded.as_bytes()).unwrap(), body);
                    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
                    zip.start_file("Camera.xml", zip::write::SimpleFileOptions::default())
                        .unwrap();
                    zip.write_all(padded.as_bytes()).unwrap();
                    assert_eq!(
                        decode_xml(&zip.finish().unwrap().into_inner()).unwrap(),
                        body
                    );
                }
            }
        }
        decode_xml(b"\xef\xbb\xbf<XML>\xff</XML>\0").unwrap_err();
    }
    #[test]
    fn zip_xml_selects_the_first_xml_file() {
        use std::io::Write as _;
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
        let first = |names: &[(&str, &str)]| {
            let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
            for (name, body) in names {
                zip.start_file(*name, zip::write::SimpleFileOptions::default())
                    .unwrap();
                zip.write_all(body.as_bytes()).unwrap();
            }
            decode_xml(&zip.finish().unwrap().into_inner())
        };
        assert_eq!(
            first(&[("a.xml", "<A/>"), ("b.xml", "<B/>")]).unwrap(),
            "<A/>"
        );
        assert_eq!(
            first(&[("readme.txt", "notes"), ("Camera.XML", "<C/>")]).unwrap(),
            "<C/>"
        );
        assert_eq!(first(&[("Camera_1_0", "<D/>")]).unwrap(), "<D/>");
        assert_eq!(
            first(&[("first.data", "<First/>"), ("second.data", "<Second/>")]).unwrap(),
            "<First/>"
        );
        for name in [
            "Camera.xml",
            "Camera.Xml",
            "Camera.xMl",
            "Camera.XML",
            "相机.XML",
        ] {
            assert_eq!(
                first(&[
                    ("readme.txt", "notes"),
                    (name, "<Selected/>"),
                    ("later.xml", "<Later/>")
                ])
                .unwrap(),
                "<Selected/>",
                "{name}"
            );
        }
        assert_eq!(
            first(&[("Camera.xml.backup", "backup"), ("Camera.xml", "<XML/>")]).unwrap(),
            "<XML/>"
        );
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        zip.add_directory("xml/", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.start_file("xml/Camera.xml", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"<E/>").unwrap();
        let nested = zip.finish().unwrap().into_inner();
        assert_eq!(decode_xml(&nested).unwrap(), "<E/>");
    }
    #[test]
    fn unavailable_and_locked_features_fail_closed() {
        let m = map(
            "<Integer Name='Feature'><pValue>R</pValue><pIsAvailable>Available</pIsAvailable><pIsLocked>Locked</pIsLocked></Integer><Integer Name='Available'><Value>1</Value></Integer><Integer Name='Locked'><Value>1</Value></Integer><IntReg Name='R'><Address>0</Address><Length>4</Length></IntReg><Integer Name='Unavailable'><pValue>R</pValue><pIsAvailable>No</pIsAvailable></Integer><Integer Name='No'><Value>0</Value></Integer>",
        );
        let mut io = Memory::default();
        m.get(&mut io, "Feature").unwrap();
        assert!(m.set(&mut io, "Feature", "3").is_err());
        m.get(&mut io, "Unavailable").unwrap_err();
    }
    #[test]
    fn features_omit_what_the_model_does_not_implement() {
        let m = map(concat!(
            "<Integer Name='Gain'><pValue>GainReg</pValue><pIsImplemented>GainImpl</pIsImplemented></Integer>",
            "<IntReg Name='GainReg'><Address>0</Address><Length>4</Length></IntReg>",
            "<Integer Name='Shutter'><pValue>ShutterReg</pValue><pIsAvailable>GainImpl</pIsAvailable></Integer>",
            "<IntReg Name='ShutterReg'><Address>8</Address><Length>4</Length></IntReg>",
            "<MaskedIntReg Name='GainImpl'><Address>4</Address><Length>4</Length><Bit>0</Bit><AccessMode>RO</AccessMode></MaskedIntReg>",
        ));
        let mut io = Memory::default();
        let names = |m: &NodeMap, io: &mut Memory| {
            m.features(io)
                .into_iter()
                .map(|feature| feature.name)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            names(&m, &mut io),
            ["GainImpl", "GainReg", "Shutter", "ShutterReg"]
        );
        m.get(&mut io, "Gain").unwrap_err();
        io.write_memory(4, &[1, 0, 0, 0]).unwrap();
        assert!(names(&m, &mut io).contains(&"Gain".to_owned()));
    }
    #[test]
    fn availability_and_locks_are_told_apart_and_traced_to_their_inputs() {
        let m = map(concat!(
            "<Integer Name='Shown'><pValue>Inner</pValue></Integer>",
            "<Integer Name='Inner'><Value>1</Value><pIsAvailable>Gate</pIsAvailable></Integer>",
            "<Integer Name='Gate'><pValue>GateValue</pValue></Integer><Integer Name='GateValue'><Value>0</Value></Integer>",
            "<Integer Name='Hidden'><Value>1</Value><pIsImplemented>Gate</pIsImplemented></Integer>",
            "<Enumeration Name='Format'><EnumEntry Name='A'><Value>0</Value></EnumEntry><EnumEntry Name='B'><Value>1</Value><pIsAvailable>Gate</pIsAvailable></EnumEntry><Value>0</Value></Enumeration>",
            "<Integer Name='Time'><Value>5</Value><pIsLocked>AutoOn</pIsLocked></Integer>",
            "<IntSwissKnife Name='AutoOn'><pVariable Name='A'>AutoValue</pVariable><Formula>A = 1</Formula></IntSwissKnife>",
            "<Enumeration Name='Auto'><EnumEntry Name='Off'><Value>0</Value></EnumEntry><EnumEntry Name='On'><Value>1</Value></EnumEntry><pValue>AutoValue</pValue></Enumeration>",
            "<Integer Name='AutoValue'><Value>1</Value></Integer><Integer Name='Other'><Value>0</Value></Integer>",
        ));
        let mut io = Memory::default();
        // Through a value link, pIsAvailable hides; pIsImplemented removes.
        assert_eq!(m.availability(&mut io, "Shown"), Availability::Unavailable);
        assert_eq!(m.availability(&mut io, "Hidden"), Availability::Absent);
        assert_eq!(m.availability(&mut io, "Missing"), Availability::Absent);
        assert_eq!(
            m.entry_availability(&mut io, "Format", "A"),
            Availability::Available
        );
        assert_eq!(
            m.entry_availability(&mut io, "Format", "B"),
            Availability::Unavailable
        );
        assert_eq!(
            m.entry_availability(&mut io, "Format", "C"),
            Availability::Absent
        );
        m.set(&mut io, "GateValue", "1").unwrap();
        assert_eq!(m.availability(&mut io, "Shown"), Availability::Available);
        assert_eq!(
            m.entry_availability(&mut io, "Format", "B"),
            Availability::Available
        );
        // A lock is released by the setting its condition reads.
        assert!(m.is_locked(&mut io, "Time"));
        assert!(m.lock_released_by("Time", &["Auto"]));
        assert!(!m.lock_released_by("Time", &["Other", "Time"]));
        m.set(&mut io, "Auto", "Off").unwrap();
        assert!(!m.is_locked(&mut io, "Time"));
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
    fn commands_report_done_when_the_target_clears_and_may_float() {
        struct SelfClearing {
            memory: Memory,
            reads: usize,
        }
        impl RegisterIo for SelfClearing {
            fn read_memory(&mut self, a: u64, n: usize) -> Result<Vec<u8>> {
                self.reads += 1;
                if self.reads == 3 {
                    self.memory.write_memory(a, &vec![0; n])?;
                }
                self.memory.read_memory(a, n)
            }
            fn write_memory(&mut self, a: u64, b: &[u8]) -> Result<()> {
                self.memory.write_memory(a, b)
            }
        }
        let m = map(concat!(
            "<Command Name='UserSetLoad'><pValue>UserSetLoadReg</pValue><CommandValue>1</CommandValue></Command>",
            "<IntReg Name='UserSetLoadReg'><Address>0x20</Address><Length>4</Length><AccessMode>RW</AccessMode></IntReg>",
            "<Command Name='TriggerSoftware'><ImposedAccessMode>WO</ImposedAccessMode><pValue>TriggerReg</pValue><CommandValue>1</CommandValue></Command>",
            "<IntReg Name='TriggerReg'><Address>0x24</Address><Length>4</Length><AccessMode>RW</AccessMode></IntReg>",
            "<Command Name='AcquisitionStart'><Value>0</Value><CommandValue>0</CommandValue></Command>",
        ));
        let timeout = std::time::Duration::from_secs(5);
        let mut io = SelfClearing {
            memory: Memory::default(),
            reads: 0,
        };
        m.execute(&mut io, "UserSetLoad").unwrap();
        assert!(m.wait_done(&mut io, "UserSetLoad", timeout).unwrap());
        assert_eq!(io.reads, 3);
        let mut io = Memory::default();
        m.execute(&mut io, "UserSetLoad").unwrap();
        let short = std::time::Duration::from_millis(20);
        assert!(!m.wait_done(&mut io, "UserSetLoad", short).unwrap());
        m.execute(&mut io, "TriggerSoftware").unwrap();
        assert!(m.wait_done(&mut io, "TriggerSoftware", short).unwrap());
        m.execute(&mut io, "AcquisitionStart").unwrap();
        assert!(m.wait_done(&mut io, "AcquisitionStart", short).unwrap());
        assert!(m.is_writable(&mut io, "AcquisitionStart"));
    }
    #[test]
    fn converter_float_and_cycles() {
        let m = map(
            "<Float Name='Exposure'><pValue>C</pValue></Float><Converter Name='C'><pValue>R</pValue><FormulaFrom>TO * 0.5</FormulaFrom><FormulaTo>FROM / 0.5</FormulaTo></Converter><FloatReg Name='R'><Address>0</Address><Length>8</Length></FloatReg><Integer Name='Loop'><pValue>Loop</pValue></Integer>",
        );
        let mut io = Memory::default();
        m.set(&mut io, "Exposure", "100").unwrap();
        assert_eq!(m.get(&mut io, "Exposure").unwrap(), 100.0);
        m.get(&mut io, "Loop").unwrap_err();
        assert_eq!(m.bounds(&mut io, "Loop").unwrap(), Bounds::default());
    }
    #[test]
    fn excess_function_arguments_are_still_parsed_before_the_count_is_rejected() {
        let vars = BTreeMap::new();
        for (expression, message) in [
            ("ROUND(1,2,3)", "wrong number of arguments"),
            ("ROUND(1,2,1/0)", "formula division by zero"),
            ("ROUND(1,2,3", "unclosed formula function"),
            ("0 && ROUND(1,2,1/0)", "wrong number of arguments"),
        ] {
            let error = evaluate::<f64>(expression, &vars).unwrap_err();
            assert!(
                error.to_string().contains(message),
                "{expression}: {error:#}"
            );
        }
        let long = format!("ROUND(1{})", ",1".repeat(2_000));
        let error = evaluate::<f64>(&long, &vars).unwrap_err();
        assert!(
            error.to_string().contains("wrong number of arguments"),
            "{error:#}"
        );
    }

    #[test]
    fn formula_operator_spellings_keep_their_meanings() {
        let vars = BTreeMap::new();
        for (expression, expected) in [
            ("5+2", 7),
            ("5-2", 3),
            ("5*2", 10),
            ("5/2", 2),
            ("5%2", 1),
            ("5&2", 0),
            ("5|2", 7),
            ("5^2", 7),
            ("~5", -6),
            ("!5", 0),
            ("+5", 5),
            ("-5", -5),
            ("5<2", 0),
            ("5>2", 1),
            ("5=2", 0),
            ("5<<2", 20),
            ("5>>2", 1),
            ("5<=5", 1),
            ("5>=5", 1),
            ("5==5", 1),
            ("5!=2", 1),
            ("5<>2", 1),
            ("5&&2", 1),
            ("0||2", 1),
            ("5**2", 25),
        ] {
            assert_eq!(
                evaluate::<i64>(expression, &vars).unwrap(),
                expected,
                "{expression}"
            );
        }
    }

    #[test]
    fn formula_expressions_are_evaluated_where_they_are_used() {
        let m = map(concat!(
            "<Float Name='Exposure'><pValue>C</pValue></Float>",
            "<Converter Name='C'><Expression Name='SCALED'>FROM*1000</Expression><pVariable Name='STEP'>Step</pVariable><Expression Name='BROKEN'>STEP/0</Expression>",
            "<FormulaTo>SCALED/STEP</FormulaTo><FormulaFrom>TO*STEP/1000</FormulaFrom><pValue>R</pValue></Converter>",
            "<Integer Name='Step'><Value>4</Value></Integer>",
            "<IntReg Name='R'><Address>0</Address><Length>4</Length><AccessMode>RW</AccessMode></IntReg>",
            "<IntSwissKnife Name='Half'><Expression Name='HALF'>X/2</Expression><pVariable Name='X'>Step2</pVariable><Formula>HALF*2</Formula></IntSwissKnife>",
            "<Integer Name='Step2'><Value>7</Value></Integer>",
            "<IntSwissKnife Name='Nested'><Expression Name='A'>1</Expression><Expression Name='B'>A+1</Expression><Formula>B</Formula></IntSwissKnife>",
        ));
        let mut io = Memory::default();
        m.set(&mut io, "Exposure", "2").unwrap();
        assert_eq!(io.read_memory(0, 4).unwrap(), 500u32.to_le_bytes());
        assert_eq!(m.get(&mut io, "Exposure").unwrap(), 2.0);
        assert_eq!(m.get(&mut io, "Half").unwrap(), 6);
        let error = format!("{:#}", m.get(&mut io, "Nested").unwrap_err());
        assert!(error.contains("unknown formula variable A"), "{error}");
    }
    #[test]
    fn formula_variable_extensions_read_bounds_and_entries() {
        let m = map(concat!(
            "<SwissKnife Name='MidRange'><pVariable Name='Gain.Max'>Gain</pVariable><pVariable Name='Gain.Min'>Gain</pVariable><Formula> (Gain.Max - Gain.Min) / 2 </Formula></SwissKnife>",
            "<Float Name='Gain'><pValue>GainRaw</pValue><Min>1.5</Min><pMax>GainMax</pMax></Float>",
            "<FloatReg Name='GainRaw'><Address>0</Address><Length>8</Length><AccessMode>RW</AccessMode></FloatReg>",
            "<Integer Name='GainMax'><Value>24</Value></Integer>",
            "<IntSwissKnife Name='Aligned'><pVariable Name='W.Value'>Width</pVariable><pVariable Name='W.Inc'>Width</pVariable><pVariable Name='MODE'>Mode</pVariable><pVariable Name='Mode.Entry.Fast'>Mode</pVariable>",
            "<Formula>MODE = Mode.Entry.Fast ? W.Value / W.Inc * W.Inc : W.Value</Formula></IntSwissKnife>",
            "<Integer Name='Width'><Value>1001</Value><Inc>16</Inc></Integer>",
            "<Enumeration Name='Mode'><EnumEntry Name='Slow'><Value>3</Value></EnumEntry><EnumEntry Name='Fast'><pValue>FastCode</pValue></EnumEntry><Value>7</Value></Enumeration>",
            "<Integer Name='FastCode'><Value>7</Value></Integer>",
            "<IntSwissKnife Name='Bad'><pVariable Name='Mode.Entry.Missing'>Mode</pVariable><Formula>Mode.Entry.Missing</Formula></IntSwissKnife>",
        ));
        let mut io = Memory::default();
        assert_eq!(m.get(&mut io, "MidRange").unwrap(), 11.25);
        assert_eq!(m.get(&mut io, "Aligned").unwrap(), 992);
        m.set(&mut io, "Mode", "Slow").unwrap();
        assert_eq!(m.get(&mut io, "Aligned").unwrap(), 1001);
        let error = format!("{:#}", m.get(&mut io, "Bad").unwrap_err());
        assert!(error.contains("Mode has no entry Missing"), "{error}");
    }
    #[test]
    fn formula_fail_closed() {
        let vars = BTreeMap::from([("X".into(), 7.0)]);
        assert_eq!(
            evaluate("(X & 3) == 3 ? 0x10 + 2 * 4 : 0", &vars).unwrap(),
            24.0
        );
        evaluate("UNKNOWN(X)", &vars).unwrap_err();
        assert_eq!(evaluate("X=7 ? EXP(LN(1)) : 0", &vars).unwrap(), 1.0);
        evaluate("X / 0", &vars).unwrap_err();
        evaluate("X + ", &vars).unwrap_err();
        for expression in ["€", "X + €", "+é", "🚀", "Xé", "1 ** 🚀"] {
            let error = evaluate(expression, &vars).unwrap_err();
            assert!(
                error.to_string().contains("non-ASCII formula syntax"),
                "{expression}: {error}"
            );
        }
    }
    #[test]
    fn standard_formula_functions_and_constants() {
        let none = BTreeMap::new();
        for (expression, expected) in [
            ("ROUND(10.11, 1)", 10.1),
            ("ROUND(10.99, 1)", 11.0),
            ("ROUND(-20.11, 1)", -20.1),
            ("ROUND(-20.99, 1)", -21.0),
            ("ROUND(1234, -2)", 1200.0),
            ("ROUND(10.5)", 11.0),
            ("LN(E)", 1.0),
            ("LG(10)", 1.0),
            ("COS(PI)", -1.0),
            ("SIN(-PI/2)", -1.0),
            ("SGN(-2.0)", -1.0),
            ("SGN(0)", 0.0),
            ("SGN(2)", 1.0),
            ("NEG(-2.5)", 2.5),
            ("exp(0) + Abs(-1)", 2.0),
        ] {
            assert_eq!(
                evaluate::<f64>(expression, &none).unwrap(),
                expected,
                "{expression}"
            );
        }
        let shadowed = BTreeMap::from([("E".into(), 2.0)]);
        assert_eq!(
            evaluate::<f64>("E * PI", &shadowed).unwrap(),
            2.0 * std::f64::consts::PI
        );
        for invalid in [
            "LOG(10)",
            "ROUND(1, 2, 3)",
            "SGN(1, 2)",
            "SGN()",
            "1, 2",
            "(1, 2)",
        ] {
            assert!(evaluate::<f64>(invalid, &none).is_err(), "{invalid}");
        }
        let integers = BTreeMap::from([("X".into(), -7i64)]);
        assert_eq!(evaluate::<i64>("SGN(X) * NEG(X)", &integers).unwrap(), -7);
        evaluate::<i64>("PI", &integers).unwrap_err();
        evaluate::<i64>("X > 0 ? LG(X) : 0", &integers).unwrap_err();

        let m = map(concat!(
            "<Float Name='ShutterTemperature'><pValue>ShutterTemperatureValConv</pValue><Unit>K</Unit></Float>",
            "<Converter Name='ShutterTemperatureValConv'><FormulaTo>(FROM - 273) * 100</FormulaTo><FormulaFrom>ROUND(((TO &gt; 32767 ? (TO - 65536) : TO) / 100),0) + 273</FormulaFrom><pValue>ShutterTempInternalReg</pValue><Slope>Varying</Slope></Converter>",
            "<IntReg Name='ShutterTempInternalReg'><Address>0xE868</Address><Length>4</Length><AccessMode>RW</AccessMode><Sign>Signed</Sign><Endianess>BigEndian</Endianess></IntReg>",
            "<Float Name='AutoExposureTimeAbsLowerLimit'><pValue>N871</pValue></Float>",
            "<Converter Name='N871'><pVariable Name='P1'>N1803</pVariable><FormulaTo>ROUND(FROM/P1,0)*P1</FormulaTo><FormulaFrom>TO</FormulaFrom><pValue>N1804</pValue><Slope>Increasing</Slope></Converter>",
            "<Integer Name='N1803'><Value>35</Value></Integer>",
            "<IntReg Name='N1804'><Address>0x100</Address><Length>4</Length><AccessMode>RW</AccessMode><Endianess>BigEndian</Endianess></IntReg>",
        ));
        let mut io = Memory::default();
        io.write_memory(0xe868, &2_549u32.to_be_bytes()).unwrap();
        assert_eq!(m.get(&mut io, "ShutterTemperature").unwrap(), 298.0);
        io.write_memory(0xe868, &65_036u32.to_be_bytes()).unwrap();
        assert_eq!(m.get(&mut io, "ShutterTemperature").unwrap(), 268.0);
        m.set(&mut io, "AutoExposureTimeAbsLowerLimit", "1000")
            .unwrap();
        assert_eq!(io.read_memory(0x100, 4).unwrap(), 1015u32.to_be_bytes());
    }
    #[test]
    fn long_selector_chains_do_not_exhaust_the_nesting_limit() {
        let m = map(concat!(
            "<Enumeration Name='PixelFormat'><pValue>N195</pValue><EnumEntry Name='Mono8'><Value>0x01080001</Value></EnumEntry><EnumEntry Name='BayerRG8'><Value>0x01080009</Value></EnumEntry><EnumEntry Name='YUV422Packed'><Value>0x0210001f</Value></EnumEntry></Enumeration>",
            "<IntConverter Name='N195'><FormulaTo>(FROM=0x01080001)?(1):((FROM=0x01100003)?(3):((FROM=0x010a0046)?(4):((FROM=0x01100005)?(5):((FROM=0x010C0006)?(6):((FROM=0x01080008)?(10):((FROM=0x01080009)?(11):((FROM=0x0108000a)?(12):((FROM=0x0108000b)?(13):((FROM=0x0110000c)?(14):((FROM=0x0110000d)?(15):((FROM=0x0110000e)?(16):((FROM=0x0110000f)?(17):((FROM=0x010a0056)?(51):((FROM=0x010a0058)?(52):((FROM=0x010a0052)?(54):((FROM=0x010a0054)?(53):((FROM=0x01100010)?(18):((FROM=0x01100011)?(20):((FROM=0x01100012)?(22):((FROM=0x01100013)?(24):((FROM=0x010c002a)?(19):((FROM=0x010c002b)?(21):((FROM=0x010c002c)?(23):((FROM=0x010c002d)?(25):((FROM=0x0210001f)?(43):((FROM=0x02100032)?(44):((FROM=0x02180014)?(30):((FROM=0x02180015)?(31):((FROM=0x02200017)?(33):(0xffffffff))))))))))))))))))))))))))))))</FormulaTo>",
            "<FormulaFrom>(TO=1)?(0x01080001):((TO=3)?(0x01100003):((TO=4)?(0x010a0046):((TO=5)?(0x01100005):((TO=6)?(0x010C0006):((TO=10)?(0x01080008):((TO=11)?(0x01080009):((TO=12)?(0x0108000a):((TO=13)?(0x0108000b):((TO=14)?(0x0110000c):((TO=15)?(0x0110000d):((TO=16)?(0x0110000e):((TO=17)?(0x0110000f):((TO=51)?(0x010a0056):((TO=52)?(0x010a0058):((TO=54)?(0x010a0052):((TO=53)?(0x010a0054):((TO=18)?(0x01100010):((TO=20)?(0x01100011):((TO=22)?(0x01100012):((TO=24)?(0x01100013):((TO=19)?(0x010c002a):((TO=21)?(0x010c002b):((TO=23)?(0x010c002c):((TO=25)?(0x010c002d):((TO=43)?(0x0210001f):((TO=44)?(0x02100032):((TO=30)?(0x02180014):((TO=31)?(0x02180015):((TO=33)?(0x02200017):(0xffffffff))))))))))))))))))))))))))))))</FormulaFrom>",
            "<pValue>N1528</pValue><Slope>Varying</Slope></IntConverter>",
            "<IntReg Name='N1528'><Address>0x30024</Address><Length>4</Length><AccessMode>RW</AccessMode><Endianess>BigEndian</Endianess></IntReg>",
        ));
        let mut io = Memory::default();
        io.write_memory(0x30024, &11u32.to_be_bytes()).unwrap();
        assert_eq!(m.get(&mut io, "PixelFormat").unwrap(), "BayerRG8");
        m.set(&mut io, "PixelFormat", "YUV422Packed").unwrap();
        assert_eq!(io.read_memory(0x30024, 4).unwrap(), 43u32.to_be_bytes());

        let chain = |levels: usize| {
            let mut formula = "(0)".to_owned();
            for level in (1..=levels).rev() {
                formula = format!("(TO={level})?({level}):({formula})");
            }
            formula
        };
        let vars = BTreeMap::from([("TO".to_owned(), 90i64)]);
        assert_eq!(evaluate(&chain(100), &vars).unwrap(), 90);
        assert_eq!(
            evaluate(&chain(100).replace(['(', ')'], ""), &vars).unwrap(),
            90
        );
        let mut flat = String::new();
        for level in 1..=1000 {
            let _ = write!(flat, "TO={level}?{level}:");
        }
        flat.push('0');
        assert_eq!(evaluate(&flat, &vars).unwrap(), 90);
        for deep in [
            format!("{}1{}", "(".repeat(200), ")".repeat(200)),
            format!("{}1", "-".repeat(200)),
            format!("2{}", "**2".repeat(200)),
            chain(200),
        ] {
            let error = evaluate::<i64>(&deep, &vars).unwrap_err().to_string();
            assert!(error.contains("nesting"), "{error}");
        }
    }
    #[test]
    fn untaken_formula_branches_cannot_fail() {
        let int = |expression: &str, vars: &[(&str, i64)]| {
            let vars = vars.iter().map(|(k, v)| (k.to_string(), *v)).collect();
            evaluate::<i64>(expression, &vars)
        };
        let range = " ( ( MONO <> 1 ) ? 255 : ( ( BPP > 12 ) && ( ISPE = 1 ) ? ( 0xFFF << ( BPP - 12 ) ) : ( ( BPP > 16 ) && ( ISPE = 0 ) ? ( 0xFFFF << ( BPP - 16 ) ) : ( ( 1 << BPP) -1 ) ) ) ) ";
        for (bpp, isp, max) in [(8, 1, 255), (12, 0, 4095), (16, 1, 65520)] {
            let vars = [("MONO", 1), ("BPP", bpp), ("ISPE", isp)];
            assert_eq!(int(range, &vars).unwrap(), max, "BPP={bpp}");
        }
        let option = "(FEAT<32)?((GEVOPT>>FEAT)&0x1):((FEAT<64)?((IPOPT>>(FEAT-32))&0x1) : ((FEAT=65)?MCCB:((FEAT=66)?SCCB:0)))";
        for (feature, supported) in [(0, 0), (5, 1), (31, 1), (33, 1), (65, 1), (66, 0), (70, 0)] {
            let vars = [
                ("FEAT", feature),
                ("GEVOPT", 0x8000_0020),
                ("IPOPT", 0b10),
                ("MCCB", 1),
                ("SCCB", 0),
            ];
            assert_eq!(int(option, &vars).unwrap(), supported, "FEAT={feature}");
        }
        assert_eq!(int("(X <> 0) && (10 / X > 1)", &[("X", 0)]).unwrap(), 0);
        assert_eq!(int("(X = 0) || (10 / X > 1)", &[("X", 0)]).unwrap(), 1);
        let none = BTreeMap::new();
        assert_eq!(evaluate::<f64>("0 ? LN(-1) * 2 : 3", &none).unwrap(), 3.0);
        int("1 ? 1 / 0 : 0", &[]).unwrap_err();
        int("0 ? UNKNOWN : 0", &[]).unwrap_err();
        int("0 ? (1 : 0", &[]).unwrap_err();
    }
    #[test]
    fn mixed_integer_float_comparisons_keep_integer_boundaries() {
        for (integer, float, equal) in [
            (json!(u64::MAX), 18_446_744_073_709_551_616.0, false),
            (json!(i64::MIN), -9_223_372_036_854_775_808.0, true),
            (
                json!(9_007_199_254_740_993u64),
                9_007_199_254_740_992.0,
                false,
            ),
            (json!(0), -0.0, true),
            (json!(1), 1.5, false),
        ] {
            let float = json!(float);
            assert_eq!(numeric_equal(&integer, &float).unwrap(), equal);
            assert_eq!(numeric_equal(&float, &integer).unwrap(), equal);
        }
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, f64::MAX, 0.5] {
            assert!(integral_float(value).is_none());
        }
        signed_integer(&json!(1.5)).unwrap_err();
        unsigned_integer(&json!(-1.0)).unwrap_err();
    }

    #[test]
    fn float_integer_conversions_keep_signed_64_bit_boundaries() {
        let upper = 9_223_372_036_854_775_808.0f64;
        let largest = 9_223_372_036_854_774_784;
        for (value, truncated, rounded) in [
            (upper.next_down(), largest, largest),
            (-upper, i64::MIN, i64::MIN),
            (0.0, 0, 0),
            (-0.0, 0, 0),
            (2.7, 2, 3),
            (-2.7, -2, -3),
        ] {
            assert_eq!(bits(value).unwrap(), truncated, "{value}");
            assert_eq!(i64::variable(&json!(value)).unwrap(), rounded, "{value}");
        }
        for value in [
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            upper,
            (-upper).next_down(),
            f64::MAX,
            -f64::MAX,
        ] {
            assert_eq!(
                bits(value).unwrap_err().to_string(),
                "bitwise formula operand outside the 64-bit integer range"
            );
            if value.is_finite() {
                assert_eq!(
                    i64::variable(&json!(value)).unwrap_err().to_string(),
                    format!("integer formula value {value} exceeds the 64-bit range")
                );
            }
        }
    }

    #[test]
    fn float_bitwise_operands_truncate_toward_zero() {
        let none = BTreeMap::new();
        assert_eq!(evaluate::<f64>("~2.7", &none).unwrap(), -3.0);
        assert_eq!(evaluate::<f64>("-2.5 & 0xFF", &none).unwrap(), 254.0);
        evaluate::<f64>("1e19 & 1", &none).unwrap_err();
        let m = map(concat!(
            "<Float Name='StrobeDelay'><pValue>StrobeDelayVal_Float</pValue><Unit>us</Unit></Float>",
            "<Converter Name='StrobeDelayVal_Float'><pVariable Name='OLD'>StrobeLineDelay_Reg</pVariable><FormulaTo>( FROM * 1.024 ) &amp; 0x0000FFFF</FormulaTo><FormulaFrom>( TO / 1.024 + 0.5 ) &amp; 0x0000FFFF</FormulaFrom><pValue>StrobeLineDelay_Reg</pValue><Slope>Increasing</Slope></Converter>",
            "<MaskedIntReg Name='StrobeLineDelay_Reg'><Address>0x1000</Address><Length>4</Length><AccessMode>RW</AccessMode><LSB>15</LSB><MSB>0</MSB><Sign>Unsigned</Sign><Endianess>BigEndian</Endianess></MaskedIntReg>",
            "<Float Name='ContrastAbs'><pValue>N907</pValue></Float>",
            "<Converter Name='N907'><FormulaTo>((FROM-1)*255)&gt;&gt;1</FormulaTo><FormulaFrom>((TO&lt;&lt;1)/255)+1</FormulaFrom><pValue>N908</pValue><Slope>Increasing</Slope></Converter>",
            "<Integer Name='N908'><pValue>N1753</pValue></Integer>",
            "<IntReg Name='N1753'><Address>0x2000</Address><Length>4</Length><AccessMode>RW</AccessMode><Endianess>BigEndian</Endianess></IntReg>",
        ));
        let mut io = Memory::default();
        io.write_memory(0x1000, &[0, 102, 0, 0]).unwrap();
        assert_eq!(m.get(&mut io, "StrobeDelay").unwrap(), 100.0);
        m.set(&mut io, "StrobeDelay", "100").unwrap();
        assert_eq!(io.read_memory(0x1000, 4).unwrap(), [0, 102, 0, 0]);
        m.set(&mut io, "ContrastAbs", "1.5").unwrap();
        assert_eq!(io.read_memory(0x2000, 4).unwrap(), 63u32.to_be_bytes());
    }
    #[test]
    fn integer_formulas_use_64_bit_integer_arithmetic() {
        let int = |expression: &str, vars: &[(&str, i64)]| {
            let vars = vars.iter().map(|(k, v)| (k.to_string(), *v)).collect();
            evaluate::<i64>(expression, &vars)
        };
        assert_eq!(int("10/4", &[]).unwrap(), 2);
        assert_eq!(int("-7/2", &[]).unwrap(), -3);
        assert_eq!(int("-7%3", &[]).unwrap(), -1);
        assert_eq!(int("FROM * 4.0", &[("FROM", 5)]).unwrap(), 20);
        assert_eq!(int("0xFFFFFFFFFFFFFFFF", &[]).unwrap(), -1);
        assert_eq!(
            int("ABS(MAXIMUM - INDEX-1)", &[("MAXIMUM", 4), ("INDEX", 10)]).unwrap(),
            7
        );
        assert_eq!(int("X * 2", &[("X", i64::MAX)]).unwrap(), -2);
        for invalid in [
            "SQRT(16)", "ROUND(1)", "2.5*4", "1/0", "1%0", "1 << 64", "2**-1",
        ] {
            assert!(int(invalid, &[]).is_err(), "{invalid}");
        }
        assert_eq!(evaluate::<f64>("10/4", &BTreeMap::new()).unwrap(), 2.5);

        let m = map(concat!(
            "<Integer Name='RThg'><pValue>RThgValExpr</pValue><Min>1</Min><Max>4294959104</Max><Inc>1</Inc></Integer>",
            "<IntSwissKnife Name='RThgValExpr'><pVariable Name='VAR_RTHGINTERNAL'>RThgInternalReg</pVariable><Formula>VAR_RTHGINTERNAL / 8192</Formula></IntSwissKnife>",
            "<IntReg Name='RThgInternalReg'><Address>0xEA10</Address><Length>4</Length><AccessMode>RO</AccessMode><Sign>Unsigned</Sign><Endianess>BigEndian</Endianess></IntReg>",
            "<IntSwissKnife Name='CurrentWidthMax'><pVariable Name='REGWIDTHMAX'>RegWidthMax</pVariable><pVariable Name='REGOFFSETX'>RegOffsetX</pVariable><pVariable Name='REGWIDTHINC'>RegWidthInc</pVariable><Formula>((REGWIDTHMAX - REGOFFSETX)/REGWIDTHINC)*REGWIDTHINC</Formula></IntSwissKnife>",
            "<Integer Name='RegWidthMax'><Value>2848</Value></Integer><Integer Name='RegOffsetX'><Value>10</Value></Integer><Integer Name='RegWidthInc'><Value>8</Value></Integer>",
            "<IntSwissKnife Name='RegTimestampValue64'><pVariable Name='LO'>RegTimestampValueLo</pVariable><pVariable Name='HI'>RegTimestampValueHi</pVariable><Formula> (HI &lt;&lt; 32) | LO </Formula></IntSwissKnife>",
            "<IntReg Name='RegTimestampValueHi'><Address>0x948</Address><Length>4</Length><AccessMode>RO</AccessMode><Sign>Unsigned</Sign><Endianess>BigEndian</Endianess></IntReg>",
            "<IntReg Name='RegTimestampValueLo'><Address>0x94C</Address><Length>4</Length><AccessMode>RO</AccessMode><Sign>Unsigned</Sign><Endianess>BigEndian</Endianess></IntReg>",
            "<IntSwissKnife Name='Low'><pVariable Name='TS'>Counter</pVariable><Formula>TS &amp; 0xFF</Formula></IntSwissKnife>",
            "<IntReg Name='Counter'><Address>0x100</Address><Length>8</Length></IntReg>",
            "<Integer Name='OffsetValue'><pValue>RegOffsetValueInt</pValue></Integer>",
            "<IntConverter Name='RegOffsetValueInt'><FormulaTo> FROM * 4.0 </FormulaTo><FormulaFrom> TO / 4.0 </FormulaFrom><pValue>RegOffsetValue</pValue><Slope>Increasing</Slope></IntConverter>",
            "<IntReg Name='RegOffsetValue'><Address>0x141D0</Address><Length>4</Length><AccessMode>RW</AccessMode><Sign>Unsigned</Sign><Endianess>BigEndian</Endianess></IntReg>",
        ));
        let mut io = Memory::default();
        io.write_memory(0xea10, &123_456u32.to_be_bytes()).unwrap();
        assert_eq!(m.get(&mut io, "RThg").unwrap(), 15);
        assert_eq!(m.get(&mut io, "CurrentWidthMax").unwrap(), 2832);
        io.write_memory(0x948, &396_000_000u32.to_be_bytes())
            .unwrap();
        io.write_memory(0x94c, &12_345u32.to_be_bytes()).unwrap();
        assert_eq!(
            m.get(&mut io, "RegTimestampValue64").unwrap(),
            json!(1_700_807_049_216_012_345i64)
        );
        io.write_memory(0x100, &u64::MAX.to_le_bytes()).unwrap();
        assert_eq!(m.get(&mut io, "Low").unwrap(), 255);
        io.write_memory(0x141d0, &7u32.to_be_bytes()).unwrap();
        assert_eq!(m.get(&mut io, "OffsetValue").unwrap(), 1);
        m.set(&mut io, "OffsetValue", "5").unwrap();
        assert_eq!(io.read_memory(0x141d0, 4).unwrap(), 20u32.to_be_bytes());
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
    #[test]
    fn inherited_integer_minima_keep_wide_increment_bases() {
        for base in [
            9_007_199_254_740_993i128,
            -9_007_199_254_740_995,
            9_223_372_036_854_775_809,
        ] {
            for minimum in [format!("<Min>{base}</Min>"), "<pMin>Minimum</pMin>".into()] {
                let sign = if base < 0 { "Signed" } else { "Unsigned" };
                let m = map(&format!(
                    "<Integer Name='Count'><pValue>Inner</pValue><Inc>2</Inc></Integer><Integer Name='Inner'><pValue>R</pValue>{minimum}</Integer><Integer Name='Minimum'><Value>{base}</Value></Integer><IntReg Name='R'><Address>0</Address><Length>8</Length><Sign>{sign}</Sign></IntReg>"
                ));
                let mut io = Memory::default();
                m.set(&mut io, "Count", &(base + 2).to_string()).unwrap();
                assert_eq!(m.get(&mut io, "Count").unwrap(), json!(base + 2));
                let before = io.bytes.clone();
                assert!(m.set(&mut io, "Count", &(base + 1).to_string()).is_err());
                assert_eq!(io.bytes, before);
            }
        }
        let copied = map(concat!(
            "<Integer Name='Count'><pValue>A</pValue><pValueCopy>B</pValueCopy><Inc>3</Inc></Integer>",
            "<Integer Name='A'><Value>9007199254740993</Value><Min>9007199254740993</Min></Integer>",
            "<Integer Name='B'><Value>9007199254740994</Value><Min>9007199254740994</Min></Integer>",
        ));
        let mut io = Memory::default();
        copied.set(&mut io, "Count", "9007199254740997").unwrap();
        for feature in ["A", "B"] {
            assert_eq!(
                copied.get(&mut io, feature).unwrap(),
                json!(9_007_199_254_740_997u64)
            );
        }
        assert!(copied.set(&mut io, "Count", "9007199254740996").is_err());
        for feature in ["A", "B"] {
            assert_eq!(
                copied.get(&mut io, feature).unwrap(),
                json!(9_007_199_254_740_997u64)
            );
        }
    }
    #[test]
    fn integer_inputs_and_limits_do_not_round_through_floating_point() {
        let m = map(concat!(
            "<Integer Name='Count'><pValue>R</pValue><Min>9007199254740992</Min><Max>9007199254741000</Max><Inc>2</Inc></Integer>",
            "<IntReg Name='R'><Address>0</Address><Length>8</Length></IntReg>",
            "<Float Name='Alias'><pValue>Count</pValue></Float>",
            "<IntReg Name='Signed'><Address>8</Address><Length>8</Length><Sign>Signed</Sign></IntReg>"
        ));
        let mut io = Memory::default();
        m.set(&mut io, "Count", "9007199254740994.0").unwrap();
        assert_eq!(
            m.get(&mut io, "Count").unwrap(),
            json!(9_007_199_254_740_994u64)
        );
        m.set(&mut io, "Count", "9.007199254741e15").unwrap();
        assert_eq!(
            m.get(&mut io, "Count").unwrap(),
            json!(9_007_199_254_741_000u64)
        );
        let before = io.bytes.clone();
        for value in [
            "9007199254740993",
            "9007199254741001",
            "9007199254740992.1",
            "18446744073709551616",
            "1e100",
            "NaN",
        ] {
            assert!(m.set(&mut io, "Count", value).is_err(), "{value}");
            assert_eq!(io.bytes, before);
        }
        assert!(m.set(&mut io, "Alias", "9007199254740992.1").is_err());
        m.set(&mut io, "Signed", "-0x8000000000000000").unwrap();
        assert_eq!(m.get(&mut io, "Signed").unwrap(), json!(i64::MIN));
        assert!(m.set(&mut io, "Signed", "-0x8000000000000001").is_err());
    }
    #[test]
    fn float_inputs_check_bounds_but_not_increment_and_reject_nonfinite_values() {
        let m = map(
            "<Float Name='Level'><pValue>R</pValue><Min>0</Min><Max>1</Max><Inc>0.1</Inc></Float><FloatReg Name='R'><Address>0</Address><Length>8</Length></FloatReg>",
        );
        let mut io = Memory::default();
        m.set(&mut io, "Level", "0.15").unwrap();
        assert_eq!(m.get(&mut io, "Level").unwrap(), 0.15);
        m.set(&mut io, "Level", "0.3").unwrap();
        let before = io.bytes.clone();
        for value in ["-0.1", "1.1", "NaN", "inf", "-inf", ""] {
            assert!(m.set(&mut io, "Level", value).is_err(), "{value}");
            assert_eq!(io.bytes, before);
        }
        let feature = m
            .features(&mut io)
            .into_iter()
            .find(|f| f.name == "Level")
            .unwrap();
        assert_eq!(feature.inc, Some(0.1));
        m.validate_set(&mut io, "Level", "0.300000001").unwrap();
    }
    #[test]
    fn preflight_does_not_mutate_values_and_allows_temporary_auto_locks() {
        let m = map(concat!(
            "<Integer Name='Locked'><Value>1</Value></Integer>",
            "<Integer Name='Level'><pValue>R</pValue><Min>0</Min><Max>10</Max><Inc>2</Inc><pIsLocked>Locked</pIsLocked></Integer>",
            "<IntReg Name='R'><Address>0</Address><Length>4</Length></IntReg>",
            "<Integer Name='Host'><Value>4</Value><Min>0</Min><Max>10</Max></Integer>",
            "<Integer Name='Readonly'><Value>4</Value><AccessMode>RO</AccessMode></Integer>"
        ));
        let mut io = Memory::default();
        m.validate_set(&mut io, "Level", "6").unwrap();
        let feature = m
            .features(&mut io)
            .into_iter()
            .find(|f| f.name == "Level")
            .unwrap();
        assert!(!feature.writable);
        assert!(io.bytes.is_empty());
        assert!(m.set(&mut io, "Level", "6").is_err());
        assert!(m.validate_set(&mut io, "Level", "5").is_err());
        assert!(m.validate_set(&mut io, "Readonly", "6").is_err());
        m.validate_set(&mut io, "Host", "8").unwrap();
        assert_eq!(m.get(&mut io, "Host").unwrap(), 4);
        m.set(&mut io, "Locked", "0").unwrap();
        m.set(&mut io, "Level", "6").unwrap();
        assert_eq!(m.get(&mut io, "Level").unwrap(), 6);
    }
    #[test]
    fn register_storage_limits_reject_overflow_and_underflow_without_writes() {
        let m = map(concat!(
            "<IntReg Name='Unsigned'><Address>0</Address><Length>1</Length></IntReg>",
            "<MaskedIntReg Name='Signed'><Address>4</Address><Length>4</Length><LSB>4</LSB><MSB>7</MSB><Sign>Signed</Sign></MaskedIntReg>",
            "<FloatReg Name='F32'><Address>8</Address><Length>4</Length></FloatReg>",
            "<FloatReg Name='F64'><Address>16</Address><Length>8</Length></FloatReg>"
        ));
        let mut io = Memory::default();
        m.set(&mut io, "Unsigned", "255").unwrap();
        io.write_memory(4, &0xa5a5_a5a5_u32.to_le_bytes()).unwrap();
        m.set(&mut io, "Signed", "-8").unwrap();
        assert_eq!(m.get(&mut io, "Signed").unwrap(), -8);
        assert_eq!(io.read_memory(4, 4).unwrap(), 0xa5a5_a585_u32.to_le_bytes());
        m.set(&mut io, "F32", "0.1").unwrap();
        let before = io.bytes.clone();
        for (feature, value) in [
            ("Unsigned", "256"),
            ("Unsigned", "-1"),
            ("Signed", "-9"),
            ("Signed", "8"),
            ("F32", "1e40"),
            ("F32", "1e-100"),
            ("F64", "1e400"),
            ("F64", "1e-400"),
        ] {
            assert!(m.set(&mut io, feature, value).is_err(), "{feature}={value}");
            assert_eq!(io.bytes, before);
        }
    }
    #[test]
    fn string_values_preserve_input_and_validate_register_byte_capacity() {
        let m = map(
            "<String Name='Host'><Value>initial</Value></String><String Name='Label'><pValue>R</pValue></String><StringReg Name='R'><Address>0</Address><Length>4</Length></StringReg>",
        );
        let mut io = Memory::default();
        for value in ["", " spaced ", "é", "a\0b"] {
            m.set(&mut io, "Host", value).unwrap();
            assert_eq!(m.get(&mut io, "Host").unwrap(), value);
        }
        m.set(&mut io, "Label", "éé").unwrap();
        assert_eq!(m.get(&mut io, "Label").unwrap(), "éé");
        let before = io.bytes.clone();
        for value in ["12345", "ééé", "a\0b"] {
            assert!(m.set(&mut io, "Label", value).is_err());
            assert_eq!(io.bytes, before);
        }
        m.set(&mut io, "Label", "").unwrap();
        assert_eq!(io.read_memory(0, 4).unwrap(), [0; 4]);
    }
    #[test]
    fn boolean_and_enum_errors_describe_valid_inputs() {
        let m = map(
            "<Boolean Name='Enabled'><Value>0</Value></Boolean><Enumeration Name='Mode'><Value>0</Value><EnumEntry Name='Slow'><Value>0</Value></EnumEntry><EnumEntry Name='Fast'><Value>1</Value></EnumEntry></Enumeration>",
        );
        let mut io = Memory::default();
        for (text, expected) in [
            ("true", true),
            ("TrUe", true),
            ("1", true),
            (" ON ", true),
            ("false", false),
            ("FaLsE", false),
            ("0", false),
            ("\u{a0}oFf\u{2003}", false),
        ] {
            m.set(&mut io, "Enabled", text).unwrap();
            assert_eq!(m.get(&mut io, "Enabled").unwrap(), expected, "{text}");
        }
        for invalid in ["yes", "no", "2", "", "trué", "ｔｒｕｅ", "true false"] {
            let error = format!("{:#}", m.set(&mut io, "Enabled", invalid).unwrap_err());
            assert!(error.contains("true or false"), "{error}");
            assert_eq!(m.get(&mut io, "Enabled").unwrap(), false);
        }
        let error = format!("{:#}", m.set(&mut io, "Mode", "unknown").unwrap_err());
        assert!(error.contains("Slow, Fast"), "{error}");
        assert_eq!(m.get(&mut io, "Mode").unwrap(), "Slow");
    }
    #[test]
    fn boolean_and_enum_readback_preserves_distinct_64_bit_values() {
        let m = map(concat!(
            "<Boolean Name='Enabled'><pValue>R</pValue><OnValue>9007199254740993</OnValue><OffValue>9007199254740992</OffValue></Boolean>",
            "<Enumeration Name='Mode'><pValue>R</pValue><EnumEntry Name='First'><Value>9007199254740992</Value></EnumEntry><EnumEntry Name='Second'><Value>9007199254740993</Value></EnumEntry></Enumeration>",
            "<IntReg Name='R'><Address>0</Address><Length>8</Length></IntReg>",
            "<Boolean Name='Invalid'><Value>1</Value><OnValue>1</OnValue><OffValue>1</OffValue></Boolean>"
        ));
        let mut io = Memory::default();
        for (symbol, enabled) in [("Second", true), ("First", false)] {
            m.set(&mut io, "Mode", symbol).unwrap();
            assert_eq!(m.get(&mut io, "Mode").unwrap(), symbol);
            assert_eq!(m.get(&mut io, "Enabled").unwrap(), enabled);
            m.set(&mut io, "Enabled", if enabled { "true" } else { "false" })
                .unwrap();
            assert_eq!(m.get(&mut io, "Mode").unwrap(), symbol);
        }
        m.get(&mut io, "Invalid").unwrap_err();
        assert!(m.set(&mut io, "Invalid", "false").is_err());
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
        ).to_owned();
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
            let _ = write!(
                xml,
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
        assert!((gain.max.unwrap() - 48.000_000_043_508_22).abs() < 1e-9);
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
    fn converter_bounds_follow_the_slope_and_integer_increment() {
        let m = map(concat!(
            "<Integer Name='Temperature'><pValue>T</pValue></Integer>",
            "<IntConverter Name='T'><FormulaTo>FROM &lt; 0 ? FROM + 65536 : FROM</FormulaTo><FormulaFrom>TO &gt; 32767 ? TO - 65536 : TO</FormulaFrom><pValue>Raw</pValue><Slope>Varying</Slope></IntConverter>",
            "<Integer Name='Raw'><pValue>R</pValue><Min>0</Min><Max>65535</Max></Integer>",
            "<IntReg Name='R'><Address>0</Address><Length>2</Length><AccessMode>RW</AccessMode></IntReg>",
            "<Integer Name='Offset'><pValue>O</pValue></Integer>",
            "<IntConverter Name='O'><FormulaTo>FROM * 4</FormulaTo><FormulaFrom>TO / 4.0</FormulaFrom><pValue>Raw</pValue><Slope>Increasing</Slope></IntConverter>",
            "<Float Name='Scaled'><pValue>S</pValue></Float>",
            "<Converter Name='S'><FormulaTo>FROM * 2</FormulaTo><FormulaFrom>TO / 2</FormulaFrom><pValue>Raw</pValue><Slope>Varying</Slope></Converter>",
        ));
        let mut io = Memory::default();
        let full = Bounds {
            inc: Some(1.0),
            ..Bounds::default()
        };
        assert_eq!(m.bounds(&mut io, "Temperature").unwrap(), full);
        m.set(&mut io, "Temperature", "-40").unwrap();
        assert_eq!(m.get(&mut io, "Temperature").unwrap(), -40);
        assert_eq!(
            m.bounds(&mut io, "Offset").unwrap(),
            bounds(0.0, 16383.0, Some(1.0))
        );
        assert_eq!(m.bounds(&mut io, "Scaled").unwrap(), Bounds::default());
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
                bounds(f64::from(lower), 1e7, Some(1.0))
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
            bounds(0.25, 15.999_755_859_375, None)
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

#[cfg(test)]
mod external_vendor_audit {
    use super::NodeMap;

    #[test]
    #[ignore = "run scripts/test-camera-xml.py to fetch pinned captured descriptions"]
    fn captured_descriptions() {
        let directory = std::env::var("CAPTUREFAB_VENDOR_XML_DIR").unwrap();
        let count: usize = std::env::var("CAPTUREFAB_VENDOR_XML_COUNT")
            .unwrap()
            .parse()
            .unwrap();
        let mut paths = std::fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        paths.sort();
        assert_eq!(paths.len(), count);
        for path in paths {
            let xml = std::fs::read_to_string(&path).unwrap();
            let map = NodeMap::parse(&xml)
                .unwrap_or_else(|error| panic!("{}: {error:#}", path.display()));
            assert_eq!(map.unresolved(), None, "{}", path.display());
            for feature in [
                "Width",
                "Height",
                "PixelFormat",
                "PayloadSize",
                "AcquisitionStart",
                "AcquisitionStop",
            ] {
                assert!(map.has(feature), "{} has no {feature}", path.display());
            }
        }
    }

    #[test]
    #[ignore = "run scripts/test-camera-xml.py to fetch pinned captured descriptions"]
    fn captured_chunk_layouts_decode_with_frozen_registers() {
        use crate::{
            transport::chunks::{ByteOrder, ChunkData},
            types::RegisterIo,
        };
        use std::time::Duration;
        struct Device {
            reads: usize,
        }
        impl RegisterIo for Device {
            fn read_memory(&mut self, _: u64, length: usize) -> anyhow::Result<Vec<u8>> {
                self.reads += 1;
                Ok(vec![255; length])
            }
            fn write_memory(&mut self, _: u64, _: &[u8]) -> anyhow::Result<()> {
                anyhow::bail!("external XML validation never writes to a device")
            }
        }
        let directory =
            std::path::PathBuf::from(std::env::var("CAPTUREFAB_VENDOR_XML_DIR").unwrap());
        for (file, counter, counter_id, timestamp_id) in [
            (
                "Basler_acA1600_20gm.xml",
                "ChunkFramecounter",
                0x8c0f_1cd8_u32,
                0xc916_13db_u32,
            ),
            (
                "PGR_BlackflyS_13Y3M.xml",
                "ChunkFrameID",
                0x0504_0003,
                0x0508_000e,
            ),
        ] {
            let xml = std::fs::read_to_string(directory.join(file)).unwrap();
            let map = NodeMap::parse(&xml).unwrap();
            let mut device = Device { reads: 0 };
            let context = map.prepare_chunk_context(&mut device, Duration::from_secs(1));
            let reads = device.reads;
            assert!(reads <= 32);
            let counter_bytes = if counter == "ChunkFramecounter" {
                u32::MAX.to_le_bytes().to_vec()
            } else {
                u64::MAX.to_le_bytes().to_vec()
            };
            let mut payload = vec![0, 0];
            for (id, bytes) in [
                (counter_id, counter_bytes),
                (timestamp_id, u64::MAX.to_le_bytes().to_vec()),
            ] {
                payload.extend_from_slice(&bytes);
                payload.extend_from_slice(&id.to_be_bytes());
                payload.extend_from_slice(&u32::try_from(bytes.len()).unwrap().to_be_bytes());
            }
            let mut chunks = ChunkData::parse(&payload, 2, 2, ByteOrder::Big, 0).unwrap();
            chunks.payload = payload.into();
            let values = map.chunk_values(&chunks, &context);
            assert_eq!(
                values["values"]["ChunkTimestamp"],
                u64::MAX,
                "{file}: {values}"
            );
            assert_eq!(
                values["values"][counter],
                if counter == "ChunkFramecounter" {
                    u64::from(u32::MAX)
                } else {
                    u64::MAX
                },
                "{file}: {values}"
            );
            assert_eq!(device.reads, reads);
        }
    }
}

#[cfg(test)]
mod lucid_encoding_tests {
    use super::NodeMap;
    use crate::types::{LucidPixelFormat, PixelEncoding};

    /// LUCID Arena `PFNC.h`/`ArenaCDefs.h`: `LUCID_Coord3D_ABCY16 0x82400403` (custom bit 31,
    /// 64 bits per pixel). The signed sibling `Coord3D_ABCY16s` is 0x82400400.
    const XML: &str = r#"<RegisterDescription><Enumeration Name="PixelFormat"><EnumEntry Name="PixelFormat_Coord3D_ABCY16"><Symbolic>Coord3D_ABCY16</Symbolic><Value>0x82400403</Value></EnumEntry><EnumEntry Name="PixelFormat_Coord3D_ABC16"><Value>0x023000b9</Value></EnumEntry><Value>0x82400403</Value></Enumeration></RegisterDescription>"#;

    #[test]
    fn lucid_abcy16_requires_identity_and_exact_static_declaration() {
        let nodes = NodeMap::parse(XML).unwrap();
        assert_eq!(
            nodes.pixel_encodings("Lucid Vision Labs"),
            [LucidPixelFormat::Coord3dAbcy16.encoding(None)]
        );
        assert_eq!(
            nodes.pixel_encodings("LUCID VISION LABS "),
            [LucidPixelFormat::Coord3dAbcy16.encoding(None)]
        );
        for vendor in [
            "Other Manufacturer",
            "IDS Imaging Development Systems GmbH",
            "Lucid",
        ] {
            assert_eq!(
                nodes.pixel_encodings(vendor),
                [] as [PixelEncoding; 0],
                "{vendor}"
            );
        }
        for changed in [
            // Signed sibling and LUCID's SaveDefs.h discrepancy are not the unsigned layout.
            XML.replace(
                "0x82400403</Value></EnumEntry>",
                "0x82400400</Value></EnumEntry>",
            ),
            XML.replace(
                "<Symbolic>Coord3D_ABCY16</Symbolic>",
                "<Symbolic>Coord3D_ABCY16s</Symbolic>",
            ),
            // Same ID field, different size (LUCID ABCY8 reuses 0x0403).
            XML.replace(
                "0x82400403</Value></EnumEntry>",
                "0x82200403</Value></EnumEntry>",
            ),
            XML.replace(
                "<Value>0x82400403</Value></EnumEntry>",
                "<pValue>Code</pValue></EnumEntry>",
            )
            .replace(
                "</RegisterDescription>",
                "<Integer Name=\"Code\"><Value>0x82400403</Value></Integer></RegisterDescription>",
            ),
        ] {
            assert_eq!(
                NodeMap::parse(&changed)
                    .unwrap()
                    .pixel_encodings("Lucid Vision Labs"),
                [] as [PixelEncoding; 0]
            );
        }
    }

    #[test]
    fn lucid_descriptor_binds_the_full_code_and_namespace() {
        let encoding = LucidPixelFormat::Coord3dAbcy16.encoding(None);
        assert_eq!(
            encoding.lucid(0x8240_0403).ok(),
            Some(LucidPixelFormat::Coord3dAbcy16)
        );
        for code in [0x8240_0400, 0x8220_0403, 0x0240_0403, 0x0230_00b9] {
            encoding.lucid(code).unwrap_err();
            encoding.check(code).unwrap_err();
        }
        encoding.ids(0x8240_0403).unwrap_err();
        assert_eq!(encoding.row_bytes(0x8240_0403, 3), Some(24));
        for namespace in [1, 1000] {
            encoding
                .with_namespace(Some(namespace))
                .check(0x8240_0403)
                .unwrap();
        }
        for namespace in [0, 2, 4, 1001] {
            encoding
                .with_namespace(Some(namespace))
                .check(0x8240_0403)
                .unwrap_err();
        }
        let json = serde_json::to_value(encoding).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"namespace":"lucid","name":"Coord3D_ABCY16","code":0x8240_0403_u32})
        );
        assert_eq!(
            serde_json::from_value::<PixelEncoding>(json).unwrap(),
            encoding
        );
    }
}

#[cfg(test)]
mod ids_encoding_tests {
    use super::*;
    use crate::types::{IdsPixelFormat, PixelEncoding};

    #[test]
    fn ids_encoding_requires_literal_named_entries_and_manufacturer_identity() {
        let xml = r#"<RegisterDescription><Enumeration Name="PixelFormat"><EnumEntry Name="Mono10g40IDS"><Value>0x4000000f</Value></EnumEntry><EnumEntry Name="Mono12g24IDS"><Value>0x4000001f</Value></EnumEntry><Value>0x4000000f</Value></Enumeration></RegisterDescription>"#;
        let nodes = NodeMap::parse(xml).unwrap();
        assert_eq!(
            nodes.pixel_encodings("Other Manufacturer"),
            [] as [PixelEncoding; 0]
        );
        assert_eq!(
            nodes.pixel_encodings("IDS Imaging Development Systems GmbH"),
            [
                IdsPixelFormat::Mono10g40.encoding(None),
                IdsPixelFormat::Mono12g24.encoding(None)
            ]
        );
        for changed in [
            xml.replace("<EnumEntry Name=\"Mono10g40IDS\">","<EnumEntry Name=\"Mono10g40IDS\"><Symbolic>WrongName</Symbolic>")
                .replace("<EnumEntry Name=\"Mono12g24IDS\">","<EnumEntry Name=\"Mono12g24IDS\"><Symbolic>WrongName12</Symbolic>"),
            xml.replace("Mono10g40IDS","WrongName").replace("Mono12g24IDS","WrongName12"),
            xml.replace("0x4000000f","0x40000010").replace("0x4000001f","0x40000020"),
            xml.replace("<Value>0x4000000f</Value>","<pValue>Code10</pValue>")
                .replace("<Value>0x4000001f</Value>","<pValue>Code12</pValue>")
                .replace("</RegisterDescription>","<Integer Name=\"Code10\"><Value>0x4000000f</Value></Integer><Integer Name=\"Code12\"><Value>0x4000001f</Value></Integer></RegisterDescription>"),
        ] {
            assert_eq!(NodeMap::parse(&changed).unwrap().pixel_encodings("IDS Imaging Development Systems GmbH"), [] as [PixelEncoding; 0]);
        }
    }
    #[test]
    fn ids_bayer_qualification_is_finite_literal_and_respects_symbolic_precedence() {
        for format in IdsPixelFormat::ALL
            .into_iter()
            .filter(|format| !format.is_monochrome())
        {
            let name = format.name();
            let code = format.code();
            let xml = format!(
                r#"<RegisterDescription><Enumeration Name="PixelFormat"><EnumEntry Name="PixelFormat_{name}"><Symbolic>{name}</Symbolic><Value>{code}</Value></EnumEntry><Value>{code}</Value></Enumeration></RegisterDescription>"#
            );
            let nodes = NodeMap::parse(&xml).unwrap();
            assert_eq!(
                nodes.pixel_encodings("IDS Imaging Development Systems GmbH"),
                [format.encoding(None)]
            );
            assert_eq!(
                nodes.pixel_encodings("Other Manufacturer"),
                [] as [PixelEncoding; 0]
            );
            // Nested entries inherit their literal Name as Symbolic. A prefixed
            // nested Name therefore needs an explicit unprefixed Symbolic.
            let without_symbolic = xml.replace(&format!("<Symbolic>{name}</Symbolic>"), "");
            assert_eq!(
                NodeMap::parse(&without_symbolic)
                    .unwrap()
                    .pixel_encodings("IDS Imaging Development Systems GmbH"),
                [] as [PixelEncoding; 0]
            );
            // The prefix fallback is for a top-level referenced EnumEntry whose
            // Symbolic is absent, not for the parser's injected nested Symbolic.
            let referenced = format!(
                r#"<RegisterDescription><Enumeration Name="PixelFormat"><pEnumEntry>PixelFormat_{name}</pEnumEntry><Value>{code}</Value></Enumeration><EnumEntry Name="PixelFormat_{name}"><Value>{code}</Value></EnumEntry></RegisterDescription>"#
            );
            assert_eq!(
                NodeMap::parse(&referenced)
                    .unwrap()
                    .pixel_encodings("IDS Imaging Development Systems GmbH"),
                [format.encoding(None)]
            );
            for changed in [
                xml.replace(
                    &format!("<Value>{code}</Value>"),
                    &format!("<Value>{}</Value>", code + 1),
                ),
                xml.replace(
                    &format!("<Value>{code}</Value>"),
                    "<pValue>Dynamic</pValue>",
                ),
                xml.replace(
                    &format!("<Symbolic>{name}</Symbolic>"),
                    "<Symbolic>OtherLayout</Symbolic>",
                ),
            ] {
                assert_eq!(
                    NodeMap::parse(&changed)
                        .unwrap()
                        .pixel_encodings("IDS Imaging Development Systems GmbH"),
                    [] as [PixelEncoding; 0]
                );
            }
        }
        let xml = r#"<RegisterDescription><Enumeration Name="PixelFormat"><EnumEntry Name="Mono10g40IDS"><Value>0x4000000f</Value></EnumEntry><EnumEntry Name="Mono12g24IDS"><Value>0x4000001f</Value></EnumEntry><EnumEntry Name="BayerRG10g40IDS"><Value>0x40000001</Value></EnumEntry><EnumEntry Name="BayerRG12g24IDS"><Value>0x40000011</Value></EnumEntry><Value>0x40000001</Value></Enumeration></RegisterDescription>"#;
        assert_eq!(
            NodeMap::parse(xml)
                .unwrap()
                .pixel_encodings("IDS Imaging Development Systems GmbH"),
            [
                IdsPixelFormat::Mono10g40,
                IdsPixelFormat::Mono12g24,
                IdsPixelFormat::BayerRg10g40,
                IdsPixelFormat::BayerRg12g24
            ]
            .map(|format| format.encoding(None))
        );
    }
}
