//! Version-specific XHTML state parsing; RAPID values remain logical text.
use super::{Api, Resource, incomplete};
use crate::types::{Categorized, ErrorKind};
use anyhow::Result;
use roxmltree::{Document, Node};
use serde_json::{Map, Value, json};

const XHTML: &str = "http://www.w3.org/1999/xhtml";
pub(super) fn media_type(api: Api, headers: &[&str]) -> Result<()> {
    if headers.len() != 1 {
        return Err(incomplete("one XHTML Content-Type is required"));
    }
    let mut parts = headers[0].split(';');
    if !parts
        .next()
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/xhtml+xml"))
    {
        return Err(incomplete("response is not application/xhtml+xml"));
    }
    let mut version = None;
    let mut charset = None;
    for part in parts {
        let (key, value) = part
            .trim()
            .split_once('=')
            .ok_or_else(|| incomplete("malformed Content-Type parameter"))?;
        let key = key.trim();
        if !token(key) {
            return Err(incomplete("invalid Content-Type parameter name"));
        }
        let value = parameter_value(value.trim())?;
        if key.eq_ignore_ascii_case("v") {
            if version.replace(value).is_some() {
                return Err(incomplete("duplicate Content-Type version"));
            }
            let expected = if api == Api::Rws1 { "1.0" } else { "2.0" };
            if value != expected {
                return Err(incomplete(
                    "Content-Type version conflicts with configured API",
                ));
            }
        } else if key.eq_ignore_ascii_case("charset")
            && (charset.replace(value).is_some() || !value.eq_ignore_ascii_case("utf-8"))
        {
            return Err(incomplete("response charset must be UTF-8"));
        }
    }
    // The explicit Accept selects the API; a missing response v parameter is
    // retained in raw HTTP semantics, never treated as automatic detection.
    Ok(())
}
fn token(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
}
fn parameter_value(value: &str) -> Result<&str> {
    if let Some(value) = value.strip_prefix('"') {
        let value = value
            .strip_suffix('"')
            .ok_or_else(|| incomplete("unbalanced Content-Type parameter"))?;
        if value
            .bytes()
            .any(|byte| !(32..127).contains(&byte) || matches!(byte, b'"' | b'\\'))
        {
            return Err(incomplete("unsupported quoted Content-Type parameter"));
        }
        return Ok(value);
    }
    if !token(value) {
        return Err(incomplete("invalid Content-Type token parameter"));
    }
    Ok(value)
}
fn document(xml: &str) -> Result<Document<'_>> {
    if xml.len() > super::MAX_BODY || xml.contains("<!DOCTYPE") || xml.contains("<!ENTITY") {
        return Err(incomplete("oversized or DTD-bearing XHTML"));
    }
    let doc = Document::parse_with_options(
        xml,
        roxmltree::ParsingOptions {
            allow_dtd: false,
            nodes_limit: 16_384,
        },
    )
    .map_err(|_| incomplete("invalid bounded XHTML"))?;
    let root = doc.root_element();
    if root.tag_name().name() != "html" || root.tag_name().namespace() != Some(XHTML) {
        return Err(incomplete("response root is not XHTML html"));
    }
    let mut elements = 0;
    for node in doc.descendants().filter(Node::is_element) {
        elements += 1;
        if elements > 4096
            || node.ancestors().take(65).count() > 64
            || node.tag_name().namespace() != Some(XHTML)
        {
            return Err(incomplete("XHTML namespace, element or depth limit failed"));
        }
    }
    Ok(doc)
}
fn one<'a, 'input: 'a>(nodes: impl Iterator<Item = Node<'a, 'input>>) -> Result<Node<'a, 'input>> {
    let mut nodes = nodes;
    let first = nodes
        .next()
        .ok_or_else(|| incomplete("required XHTML state/resource/field is missing"))?;
    if nodes.next().is_some() {
        return Err(incomplete("duplicate XHTML state/resource/field"));
    }
    Ok(first)
}
fn named(node: Node<'_, '_>, name: &str) -> bool {
    node.is_element() && node.tag_name().name() == name
}
fn class(node: Node<'_, '_>, value: &str) -> bool {
    node.attribute("class")
        .is_some_and(|classes| classes.split_ascii_whitespace().any(|class| class == value))
}
fn state<'a, 'input: 'a>(doc: &'a Document<'input>) -> Result<Node<'a, 'input>> {
    let body = one(doc
        .root_element()
        .children()
        .filter(|node| named(*node, "body")))?;
    for status in body
        .descendants()
        .filter(|node| named(*node, "div") && class(*node, "status"))
    {
        if let Some(code) = optional(status, "code")? {
            let code = code.trim();
            if !code.is_empty() && code.parse::<i64>().map_or(true, |code| code < 0) {
                return Err(incomplete(
                    "successful HTTP response carries a controller error status",
                ));
            }
        } else if status.children().any(|node| node.is_element()) {
            return Err(incomplete("controller status container omits its code"));
        }
    }
    one(body
        .children()
        .filter(|node| named(*node, "div") && class(*node, "state")))
}
fn resource<'a, 'input: 'a>(
    state: Node<'a, 'input>,
    expected: &str,
    title: Option<&str>,
) -> Result<Node<'a, 'input>> {
    let list = one(state.children().filter(|node| named(*node, "ul")))?;
    let node = one(list
        .children()
        .filter(|node| named(*node, "li") && class(*node, expected)))?;
    if title.is_some_and(|title| node.attribute("title") != Some(title)) {
        return Err(incomplete(
            "symbol resource title does not match the configured symbol",
        ));
    }
    Ok(node)
}
fn text(node: Node<'_, '_>, limit: usize) -> Result<String> {
    if node.children().any(|child| child.is_element()) {
        return Err(incomplete("scalar XHTML field contains child elements"));
    }
    let mut text = String::new();
    for value in node
        .children()
        .filter(Node::is_text)
        .filter_map(|child| child.text())
    {
        if text.len().saturating_add(value.len()) > limit {
            return Err(incomplete("XHTML text exceeds field limit"));
        }
        text.push_str(value);
    }
    Ok(text)
}
fn field(node: Node<'_, '_>, name: &str, limit: usize) -> Result<String> {
    text(
        one(node
            .children()
            .filter(|child| named(*child, "span") && class(*child, name)))?,
        limit,
    )
}
fn optional(node: Node<'_, '_>, name: &str) -> Result<Option<String>> {
    let mut fields = node
        .children()
        .filter(|child| named(*child, "span") && class(*child, name));
    let result = fields.next().map(|node| text(node, 4096)).transpose()?;
    if fields.next().is_some() {
        return Err(incomplete("duplicate optional XHTML field"));
    }
    Ok(result)
}
pub(super) fn parse(
    api: Api,
    kind: Resource,
    properties: bool,
    symbol: Option<&str>,
    xml: &str,
) -> Result<Value> {
    let doc = document(xml)?;
    let state = state(&doc)?;
    match kind {
        Resource::ControllerState | Resource::OperationMode => {
            let (container, key, values): (&str, &str, &[&str]) =
                if kind == Resource::ControllerState {
                    (
                        "pnl-ctrlstate",
                        "ctrlstate",
                        &[
                            "init",
                            "motoron",
                            "motoroff",
                            "guardstop",
                            "emergencystop",
                            "emergencystopreset",
                            "sysfail",
                        ],
                    )
                } else {
                    (
                        "pnl-opmode",
                        "opmode",
                        &[
                            "INIT", "AUTO_CH", "MANF_CH", "MANR", "MANF", "AUTO", "UNDEF",
                        ],
                    )
                };
            let value = field(resource(state, container, None)?, key, 4096)?;
            Ok(json!({key:&value,"known":values.contains(&value.as_str())}))
        }
        Resource::RapidExecution => {
            let node = if api == Api::Rws1 {
                resource(state, "rap-execution", None)?
            } else {
                state
            };
            let control = field(node, "ctrlexecstate", 4096)?;
            let key = if api == Api::Rws1 {
                "cycle"
            } else {
                "rapidexeccycle"
            };
            let cycle = field(node, key, 4096)?;
            let mut result = json!({"ctrlexecstate":control,"ctrlexecstate_known":matches!(control.as_str(),"running"|"stopped"),key:cycle,format!("{key}_known"):matches!(cycle.as_str(),"forever"|"asis"|"once"|"oncedone")});
            if api == Api::Rws2 {
                let hold = field(node, "hdtrun", 4096)?;
                result["hdtrun_known"] = json!(matches!(hold.as_str(), "true" | "false"));
                if matches!(hold.as_str(), "true" | "false") {
                    result["hdtrun"] = json!(hold == "true");
                }
                result["hdtrun_text"] = json!(hold);
            }
            Ok(result)
        }
        Resource::SystemInfo => {
            let node = resource(
                state,
                if api == Api::Rws1 {
                    "sys-system-li"
                } else {
                    "sys-system"
                },
                None,
            )?;
            let mut values = Map::new();
            for key in ["name", "rwversion", "rwversionname", "sysid", "starttm"] {
                values.insert(key.into(), json!(field(node, key, 4096)?));
            }
            for key in [
                "major",
                "minor",
                "build",
                "revision",
                "sub-revision",
                "buildtag",
                "robapi-compatibility-revision",
                "title",
                "type",
                "description",
                "date",
                "mctimestamp",
            ] {
                if let Some(value) = optional(node, key)? {
                    values.insert(key.into(), json!(value));
                }
            }
            Ok(Value::Object(values))
        }
        Resource::RapidSymbol => {
            let symbol =
                symbol.ok_or_else(|| incomplete("symbol path missing from parser plan"))?;
            let node = resource(
                state,
                if properties {
                    "rap-sympropvar"
                } else {
                    "rap-data"
                },
                Some(symbol),
            )?;
            if !properties {
                return Ok(json!({"symbol_url":symbol,"value_text":field(node,"value",64*1024)?}));
            }
            let mut values = Map::new();
            for key in ["symburl", "dattyp", "ndim", "dim", "symtyp", "typurl"] {
                values.insert(key.into(), json!(field(node, key, 4096)?));
            }
            if values["symburl"].as_str() != Some(symbol) {
                return Err(incomplete(
                    "symbol properties name differs from configured symbol",
                ));
            }
            for key in [
                "named", "heap", "linked", "local", "ro", "taskvar", "storage",
            ] {
                if let Some(value) = optional(node, key)? {
                    values.insert(key.into(), json!(value));
                }
            }
            Ok(Value::Object(values))
        }
    }
}
pub(super) fn http_error(
    status: u16,
    body: &[u8],
    redact: impl Fn(&str) -> String,
) -> anyhow::Error {
    let kind = match status {
        401 => ErrorKind::AuthRejected,
        403 => ErrorKind::PermissionDenied,
        404 | 429 | 503 => ErrorKind::Unavailable,
        300..=399 | 406 => ErrorKind::Unsupported,
        _ => ErrorKind::OperationFailed,
    };
    // Auth failures must not echo a server's credential-reflecting body.
    let mut details = json!({"http_status":status});
    if status != 401
        && status != 403
        && let Ok(xml) = std::str::from_utf8(body)
        && let Ok(doc) = document(xml)
        && let Ok(node) = one(doc
            .descendants()
            .filter(|node| named(*node, "div") && class(*node, "status")))
        && let (Ok(code), Ok(message)) = (field(node, "code", 256), field(node, "msg", 4096))
    {
        details["abb_rws_status"] = json!({"code_text":redact(&code),"message":redact(&message)});
    }
    Categorized::new(kind, "ABB RWS controller rejected its read-only GET")
        .details(details)
        .into()
}
