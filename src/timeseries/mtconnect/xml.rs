//! Bounded namespace-aware XML views; native values and condition activations stay opaque.
use super::{Work, incomplete};
use crate::types::{Categorized, ErrorKind};
use anyhow::Result;
use roxmltree::{Document, Node};
use serde::{
    Serialize, Serializer,
    ser::{SerializeMap as _, SerializeSeq as _},
};
use std::collections::{BTreeMap, BTreeSet};

const VERSIONS: &[&str] = &[
    "1.2", "1.3", "1.4", "1.5", "1.6", "1.7", "1.8", "2.0", "2.1", "2.2", "2.3", "2.4", "2.5",
    "2.6", "2.7",
];
const MAX_NODES: usize = 32_768;

pub(super) fn document<'a>(input: &'a str, work: &mut Work) -> Result<Document<'a>> {
    if input.len() > super::MAX_BODY {
        return Err(incomplete("oversized XML"));
    }
    let left = MAX_NODES
        .checked_sub(work.nodes)
        .filter(|n| *n != 0)
        .ok_or_else(|| incomplete("XML work budget exceeded"))?;
    let doc = Document::parse_with_options(
        input,
        roxmltree::ParsingOptions {
            allow_dtd: false,
            nodes_limit: u32::try_from(left)?,
        },
    )
    .map_err(|_| incomplete("invalid bounded XML"))?;
    work.nodes += doc.descendants().count();
    if work.nodes > MAX_NODES {
        return Err(incomplete("XML node work limit exceeded"));
    }
    for node in doc.descendants().filter(Node::is_element) {
        if node.ancestors().take(65).count() > 64 || node.attributes().len() > 64 {
            return Err(incomplete("XML depth or attribute limit exceeded"));
        }
    }
    namespace(doc.root_element())?;
    Ok(doc)
}
fn namespace<'a>(node: Node<'a, '_>) -> Result<(&'a str, &'a str)> {
    let name = node.tag_name();
    let uri = name
        .namespace()
        .ok_or_else(|| incomplete("document has no MTConnect namespace"))?;
    let tail = uri
        .strip_prefix("urn:mtconnect.org:")
        .ok_or_else(|| incomplete("document namespace is not MTConnect"))?;
    let (family, version) = tail
        .split_once(':')
        .ok_or_else(|| incomplete("invalid MTConnect namespace"))?;
    if !VERSIONS.contains(&version) {
        return Err(Categorized::error(
            ErrorKind::Unsupported,
            "MTConnect namespace version is not qualified",
        ));
    }
    if family != name.name()
        || !matches!(
            family,
            "MTConnectDevices" | "MTConnectStreams" | "MTConnectError"
        )
    {
        return Err(incomplete("MTConnect root and namespace family disagree"));
    }
    Ok((family, version))
}
fn required<'a>(node: Node<'a, '_>, name: &str) -> Result<&'a str> {
    node.attribute(name)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| incomplete("required XML attribute is absent or empty"))
}
fn unsigned(node: Node<'_, '_>, name: &str) -> Result<u64> {
    let value = required(node, name)?;
    if !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(incomplete("native unsigned integer is invalid"));
    }
    value
        .parse()
        .map_err(|_| incomplete("native unsigned integer is out of range"))
}
fn child<'a, 'input>(parent: Node<'a, 'input>, name: &str) -> Result<Node<'a, 'input>> {
    let mut nodes = parent.children().filter(|n| {
        n.is_element()
            && n.tag_name().namespace() == parent.tag_name().namespace()
            && n.tag_name().name() == name
    });
    let node = nodes
        .next()
        .ok_or_else(|| incomplete("required XML container is absent"))?;
    if nodes.next().is_some() {
        return Err(incomplete("duplicate XML container"));
    }
    Ok(node)
}
#[derive(Clone, Copy)]
pub(super) struct Attributes<'a, 'input>(pub Node<'a, 'input>);
impl Serialize for Attributes<'_, '_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.0.attributes().len()))?;
        let mut attributes: Vec<_> = self.0.attributes().collect();
        attributes
            .sort_unstable_by(|a, b| (a.namespace(), a.name()).cmp(&(b.namespace(), b.name())));
        for attr in attributes {
            map.serialize_entry(&AttributeName(attr), attr.value())?;
        }
        map.end()
    }
}
struct AttributeName<'a, 'input>(roxmltree::Attribute<'a, 'input>);
impl std::fmt::Display for AttributeName<'_, '_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(uri) = self.0.namespace() {
            write!(f, "{{{uri}}}")?;
        }
        f.write_str(self.0.name())
    }
}
impl Serialize for AttributeName<'_, '_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}
#[derive(Serialize)]
pub(super) struct QName<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    namespace: Option<&'a str>,
    name: &'a str,
}
fn qname<'a>(node: Node<'a, '_>) -> QName<'a> {
    QName {
        namespace: node.tag_name().namespace(),
        name: node.tag_name().name(),
    }
}
#[derive(Clone, Copy)]
pub(super) struct Element<'a, 'input>(Node<'a, 'input>);
impl Serialize for Element<'_, '_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct View<'a, 'input> {
            qname: QName<'a>,
            attributes: Attributes<'a, 'input>,
            content: Content<'a, 'input>,
        }
        View {
            qname: qname(self.0),
            attributes: Attributes(self.0),
            content: Content(self.0),
        }
        .serialize(serializer)
    }
}
#[derive(Clone, Copy)]
pub(super) struct Content<'a, 'input>(Node<'a, 'input>);
impl Serialize for Content<'_, '_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(None)?;
        for child in self.0.children() {
            if child.is_element() {
                sequence.serialize_element(&Element(child))?;
            } else if child.is_text() {
                sequence.serialize_element(child.text().unwrap_or_default())?;
            }
        }
        sequence.end()
    }
}
#[derive(Clone, Copy)]
pub(super) struct Header<'a, 'input> {
    pub node: Node<'a, 'input>,
    pub instance: u64,
    pub buffer: u64,
    pub first: Option<u64>,
    pub last: Option<u64>,
    pub next: Option<u64>,
}
fn header<'a, 'input>(root: Node<'a, 'input>, stream: bool) -> Result<Header<'a, 'input>> {
    let node = child(root, "Header")?;
    for name in ["creationTime", "sender", "version"] {
        required(node, name)?;
    }
    let (family, version) = namespace(root)?;
    if family != "MTConnectError" && !matches!(version, "1.2" | "1.3" | "1.4" | "1.5" | "1.6") {
        required(node, "deviceModelChangeTime")?;
    }
    let instance = unsigned(node, "instanceId")?;
    let buffer = unsigned(node, "bufferSize")?;
    if buffer == 0 || buffer > u64::from(u32::MAX) {
        return Err(incomplete("bufferSize is not a positive uint32"));
    }
    let mut value = Header {
        node,
        instance,
        buffer,
        first: None,
        last: None,
        next: None,
    };
    if stream {
        let first = unsigned(node, "firstSequence")?;
        let last = unsigned(node, "lastSequence")?;
        let next = unsigned(node, "nextSequence")?;
        if first == 0
            || next == 0
            || last
                .checked_add(1)
                .is_none_or(|end| first > end || next < first || next > end)
        {
            return Err(incomplete("invalid stream sequence window"));
        }
        value.first = Some(first);
        value.last = Some(last);
        value.next = Some(next);
    }
    Ok(value)
}
#[derive(Serialize)]
pub(super) struct Item<'a, 'input> {
    pub device_uuid: &'a str,
    pub device_name: &'a str,
    pub component_id: &'a str,
    pub category: &'a str,
    pub attributes: Attributes<'a, 'input>,
    #[serde(skip)]
    component: Node<'a, 'input>,
}
pub(super) struct Probe<'a, 'input> {
    pub header: Header<'a, 'input>,
    pub version: &'a str,
    pub items: Vec<Item<'a, 'input>>,
    pub fingerprint: Canonical<'a, 'input>,
}
pub(super) fn probe<'a, 'input>(document: &'a Document<'input>) -> Result<Probe<'a, 'input>> {
    let root = document.root_element();
    let (family, version) = namespace(root)?;
    if family == "MTConnectError" {
        return Err(agent_error(root, 200)?);
    }
    if family != "MTConnectDevices" {
        return Err(incomplete("probe is not a Devices document"));
    }
    let header = header(root, false)?;
    let devices = child(root, "Devices")?;
    let mut seen_devices = BTreeSet::new();
    let mut seen_components = BTreeSet::new();
    let mut items = BTreeMap::new();
    for device in devices.children().filter(Node::is_element) {
        if device.tag_name().namespace() != root.tag_name().namespace()
            || !matches!(device.tag_name().name(), "Device" | "Agent")
        {
            return Err(incomplete("invalid Device container"));
        }
        let uuid = required(device, "uuid")?;
        let device_name = required(device, "name")?;
        if !seen_devices.insert(uuid) || seen_devices.len() > 32 {
            return Err(incomplete("duplicate or excessive Devices"));
        }
        for component in device.descendants().filter(|n| {
            n.is_element() && n.attribute("id").is_some() && n.tag_name().name() != "DataItem"
        }) {
            let id = required(component, "id")?;
            // Configuration entries can carry IDs too; only actual DataItems parents
            // become component bindings below. Duplicate relevant IDs are checked there.
            let containers = component
                .children()
                .filter(|n| {
                    n.is_element()
                        && n.tag_name().namespace() == root.tag_name().namespace()
                        && n.tag_name().name() == "DataItems"
                })
                .count();
            if containers > 1 || (containers == 1 && !seen_components.insert(id)) {
                return Err(incomplete("duplicate component ID or DataItems container"));
            }
        }
        for item in device.descendants().filter(|n| {
            n.is_element()
                && n.tag_name().namespace() == root.tag_name().namespace()
                && n.tag_name().name() == "DataItem"
        }) {
            let id = required(item, "id")?;
            let parent = item
                .parent_element()
                .filter(|n| {
                    n.tag_name().name() == "DataItems"
                        && n.tag_name().namespace() == root.tag_name().namespace()
                })
                .and_then(|n| n.parent_element())
                .ok_or_else(|| incomplete("DataItem lacks component parent"))?;
            let component_id = required(parent, "id")?;
            let category = required(item, "category")?;
            required(item, "type")?;
            if !matches!(category, "SAMPLE" | "EVENT" | "CONDITION") {
                return Err(incomplete("DataItem category is invalid"));
            }
            if items
                .insert(
                    id,
                    Item {
                        device_uuid: uuid,
                        device_name,
                        component_id,
                        category,
                        attributes: Attributes(item),
                        component: parent,
                    },
                )
                .is_some()
                || items.len() > 2048
            {
                return Err(incomplete("duplicate or excessive DataItems"));
            }
        }
    }
    Ok(Probe {
        header,
        version,
        items: items.into_values().collect(),
        fingerprint: Canonical(devices),
    })
}
#[derive(Serialize)]
pub(super) struct Observation<'a, 'input> {
    device_uuid: &'a str,
    device_name: &'a str,
    component_id: &'a str,
    category: &'a str,
    qname: QName<'a>,
    attributes: Attributes<'a, 'input>,
    text: Text<'a, 'input>,
    content: Content<'a, 'input>,
}
struct Text<'a, 'input>(Node<'a, 'input>);
impl std::fmt::Display for Text<'_, '_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for n in self.0.children().filter(Node::is_text) {
            f.write_str(n.text().unwrap_or_default())?;
        }
        Ok(())
    }
}
impl Serialize for Text<'_, '_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}
pub(super) struct Streams<'a, 'input> {
    pub header: Header<'a, 'input>,
    pub observations: Vec<Observation<'a, 'input>>,
}
pub(super) fn streams<'a, 'input>(
    document: &'a Document<'input>,
    model: &Probe<'_, '_>,
    range: Option<(u64, u64)>,
) -> Result<Streams<'a, 'input>> {
    let root = document.root_element();
    let (family, version) = namespace(root)?;
    if family == "MTConnectError" {
        return Err(agent_error(root, 200)?);
    }
    if family != "MTConnectStreams" || version != model.version {
        return Err(incomplete("stream namespace differs from probe"));
    }
    let header = header(root, true)?;
    if header.instance != model.header.instance {
        return Err(incomplete("stream agent instance differs from probe"));
    }
    let first = header
        .first
        .ok_or_else(|| incomplete("missing firstSequence"))?;
    let last = header
        .last
        .ok_or_else(|| incomplete("missing lastSequence"))?;
    let next = header
        .next
        .ok_or_else(|| incomplete("missing nextSequence"))?;
    if let Some((from, _)) = range
        && (from < first || from > last || next <= from)
    {
        return Err(incomplete("sample range or progress is invalid"));
    }
    let container = child(root, "Streams")?;
    let mut seen = BTreeSet::new();
    let mut observations = Vec::new();
    for device in container.children().filter(Node::is_element) {
        if device.tag_name().namespace() != root.tag_name().namespace()
            || device.tag_name().name() != "DeviceStream"
        {
            return Err(incomplete("invalid DeviceStream"));
        }
        let uuid = required(device, "uuid")?;
        let device_name = required(device, "name")?;
        for component in device.children().filter(Node::is_element) {
            if component.tag_name().namespace() != root.tag_name().namespace()
                || component.tag_name().name() != "ComponentStream"
            {
                return Err(incomplete("invalid ComponentStream"));
            }
            let component_id = required(component, "componentId")?;
            required(component, "component")?;
            let mut groups = BTreeSet::new();
            for group in component.children().filter(Node::is_element) {
                if !groups.insert(group.tag_name().name()) {
                    return Err(incomplete("duplicate observation category container"));
                }
                if group.tag_name().namespace() != root.tag_name().namespace() {
                    return Err(incomplete("invalid observation group namespace"));
                }
                let category = match group.tag_name().name() {
                    "Samples" => "SAMPLE",
                    "Events" => "EVENT",
                    "Condition" => "CONDITION",
                    _ => return Err(incomplete("invalid observation category group")),
                };
                for node in group.children().filter(Node::is_element) {
                    let id = required(node, "dataItemId")?;
                    let item = model
                        .items
                        .iter()
                        .find(|i| i.attributes.0.attribute("id") == Some(id))
                        .ok_or_else(|| {
                            incomplete("observation DataItem ID is absent from probe")
                        })?;
                    if item.category != category
                        || item.device_uuid != uuid
                        || item.device_name != device_name
                        || item.component_id != component_id
                    {
                        return Err(incomplete(
                            "observation device/component/category differs from probe",
                        ));
                    }
                    if item.component.tag_name().namespace()
                        == item.attributes.0.tag_name().namespace()
                        && component.attribute("component")
                            != Some(item.component.tag_name().name())
                    {
                        return Err(incomplete("ComponentStream type differs from probe"));
                    }
                    for key in ["name", "subType"] {
                        if node
                            .attribute(key)
                            .is_some_and(|v| item.attributes.0.attribute(key) != Some(v))
                        {
                            return Err(incomplete("observation metadata differs from probe"));
                        }
                    }
                    if category == "CONDITION"
                        && node.tag_name().namespace() == root.tag_name().namespace()
                        && node.attribute("type") != item.attributes.0.attribute("type")
                    {
                        return Err(incomplete("Condition type differs from probe"));
                    }
                    required(node, "timestamp")?;
                    let sequence = unsigned(node, "sequence")?;
                    if sequence == 0
                        || sequence > last
                        || !seen.insert(sequence)
                        || range.is_some_and(|(from, _)| sequence < from || sequence >= next)
                    {
                        return Err(incomplete("invalid or duplicate observation sequence"));
                    }
                    observations.push(Observation {
                        device_uuid: uuid,
                        device_name,
                        component_id,
                        category,
                        qname: qname(node),
                        attributes: Attributes(node),
                        text: Text(node),
                        content: Content(node),
                    });
                    let cap =
                        range.map_or(4096, |(_, count)| usize::try_from(count).unwrap_or(1024));
                    if observations.len() > cap {
                        return Err(incomplete("observation count exceeds admitted request"));
                    }
                }
            }
        }
    }
    Ok(Streams {
        header,
        observations,
    })
}
pub(super) fn http_failure(status: u16) -> anyhow::Error {
    Categorized::new(ErrorKind::OperationFailed, "MTConnect HTTP read failed")
        .details(serde_json::json!({"http_status":status}))
        .into()
}
pub(super) fn remote_error(status: u16, input: &str, work: &mut Work) -> Result<anyhow::Error> {
    let Ok(doc) = document(input, work) else {
        return Ok(http_failure(status));
    };
    let root = doc.root_element();
    if namespace(root)?.0 == "MTConnectError" {
        Ok(agent_error(root, status).unwrap_or_else(|_| http_failure(status)))
    } else {
        Ok(http_failure(status))
    }
}
fn agent_error(root: Node<'_, '_>, status: u16) -> Result<anyhow::Error> {
    let mut direct = None;
    let mut wrapper = None;
    for node in root.children().filter(Node::is_element) {
        if node.tag_name().namespace() != root.tag_name().namespace() {
            return Err(incomplete("invalid error element namespace"));
        }
        match node.tag_name().name() {
            "Header" => {}
            "Errors" => {
                if wrapper.replace(node).is_some() {
                    return Err(incomplete("duplicate Errors container"));
                }
            }
            "Error" => {
                if direct.replace(node).is_some() {
                    return Err(incomplete("duplicate legacy Error"));
                }
            }
            _ => return Err(incomplete("invalid MTConnect error document")),
        }
    }
    let nodes: Vec<_> = match (direct, wrapper) {
        (Some(node), None) => vec![node],
        (None, Some(container)) => container.children().filter(Node::is_element).collect(),
        _ => return Err(incomplete("missing or mixed Error/Errors")),
    };
    let mut codes = Vec::new();
    for node in nodes {
        if node.tag_name().namespace() != root.tag_name().namespace()
            || node.tag_name().name() != "Error"
        {
            return Err(incomplete("invalid MTConnect error document"));
        }
        let code = required(node, "errorCode")?;
        if code.len() > 64
            || !code.bytes().all(|b| b.is_ascii_uppercase() || b == b'_')
            || codes.len() >= 32
        {
            return Err(incomplete("invalid or excessive MTConnect error codes"));
        }
        codes.push(code);
    }
    if codes.is_empty() {
        return Err(incomplete("empty MTConnect error document"));
    }
    Ok(Categorized::new(
        ErrorKind::OperationFailed,
        "MTConnect agent rejected its read",
    )
    .details(serde_json::json!({"http_status":status,"mtconnect_error_codes":codes}))
    .into())
}
fn token(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}
pub(super) fn media_type(headers: &[(String, String)]) -> Result<()> {
    let mut found = false;
    for (_, value) in headers.iter().filter(|(name, _)| name == "content-type") {
        let mut parts = value.split(';');
        let kind = parts.next().unwrap_or_default().trim();
        if !matches!(
            kind.to_ascii_lowercase().as_str(),
            "application/xml" | "text/xml"
        ) || found
        {
            return Err(incomplete("conflicting or non-XML Content-Type"));
        }
        found = true;
        let mut seen = BTreeSet::new();
        for part in parts {
            let (key, value) = part
                .trim()
                .split_once('=')
                .ok_or_else(|| incomplete("invalid XML media parameter"))?;
            let key = key.trim();
            if !token(key) || !seen.insert(key.to_ascii_lowercase()) {
                return Err(incomplete("invalid or duplicate XML media parameter name"));
            }
            let value = value.trim();
            let value = if let Some(quoted) = value.strip_prefix('"') {
                let quoted = quoted
                    .strip_suffix('"')
                    .ok_or_else(|| incomplete("unbalanced XML media parameter quote"))?;
                if quoted
                    .bytes()
                    .any(|b| !(32..127).contains(&b) || matches!(b, b'"' | b'\\'))
                {
                    return Err(incomplete("unsupported quoted XML media parameter"));
                }
                quoted
            } else {
                if !token(value) {
                    return Err(incomplete("invalid XML media parameter token"));
                }
                value
            };
            if key.eq_ignore_ascii_case("charset") && !value.eq_ignore_ascii_case("utf-8") {
                return Err(Categorized::error(
                    ErrorKind::Unsupported,
                    "MTConnect XML charset is not UTF-8",
                ));
            }
        }
    }
    if !found {
        return Err(incomplete("XML Content-Type is absent"));
    }
    Ok(())
}

/// Stable model hash view: excludes Header clocks, comments and formatting-only text.
pub(super) struct Canonical<'a, 'input>(Node<'a, 'input>);
impl Serialize for Canonical<'_, '_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct View<'a, 'input> {
            qname: QName<'a>,
            attributes: Attributes<'a, 'input>,
            content: CanonicalContent<'a, 'input>,
        }
        View {
            qname: qname(self.0),
            attributes: Attributes(self.0),
            content: CanonicalContent(self.0),
        }
        .serialize(serializer)
    }
}
struct CanonicalContent<'a, 'input>(Node<'a, 'input>);
impl Serialize for CanonicalContent<'_, '_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(None)?;
        for child in self.0.children() {
            if child.is_element() {
                sequence.serialize_element(&Canonical(child))?;
            } else if child.is_text() {
                let structural = self
                    .0
                    .tag_name()
                    .namespace()
                    .is_some_and(|ns| ns.starts_with("urn:mtconnect.org:MTConnectDevices:"))
                    && matches!(
                        self.0.tag_name().name(),
                        "Devices" | "Device" | "Agent" | "Components" | "DataItems" | "DataItem"
                    );
                if !structural || child.text().is_some_and(|text| !text.trim().is_empty()) {
                    sequence.serialize_element(child.text().unwrap_or_default())?;
                }
            }
        }
        sequence.end()
    }
}
