//! Per-session namespace URI resolution; the Read service still uses indexed `NodeIds`.
use super::{MAX_MESSAGE, Node, timeout_error};
use crate::types::{Categorized, ErrorKind};
use anyhow::Result;
use opcua::types::{AttributeId, DataValue, NodeId, ReadValueId, Variant, VariantScalarTypeId};
use serde_json::json;
use std::collections::BTreeMap;
use tokio::time::Instant;

const STANDARD: &str = "http://opcfoundation.org/UA/";
const MAX_URI: usize = 4096;
const MAX_NAMESPACES: usize = 65_536;

pub(super) fn required(nodes: &[Node]) -> bool {
    nodes.iter().any(|node| node.namespace_uri.is_some())
}
fn valid_uri(uri: &str) -> bool {
    !uri.is_empty()
        && uri.len() <= MAX_URI
        && !uri
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
}
fn request(node_id: NodeId) -> ReadValueId {
    ReadValueId {
        node_id,
        attribute_id: AttributeId::Value as u32,
        ..Default::default()
    }
}
pub(super) fn array_request() -> ReadValueId {
    request(NodeId::new(0, 2255_u32))
}
pub(super) fn prepare(nodes: &[Node]) -> Result<Vec<ReadValueId>> {
    nodes.iter().enumerate().map(|(index, node)| {
        if let Some(uri) = &node.namespace_uri {
            if !valid_uri(uri) {
                return Err(Categorized::new(ErrorKind::Usage,
                    "OPC-UA namespace_uri must contain 1–4096 bytes without whitespace or control characters")
                    .details(json!({"node_index":index})).into());
            }
            if !["i=", "s=", "g=", "b="].iter().any(|prefix| node.node_id.starts_with(prefix)) {
                return Err(Categorized::new(ErrorKind::Usage,
                    "OPC-UA namespace_uri requires identifier-only node_id (i=, s=, g= or b=); namespace/server prefixes cannot be combined")
                    .details(json!({"node_index":index})).into());
            }
        }
        let node_id = node.node_id.parse::<NodeId>().map_err(|_| {
            Categorized::new(ErrorKind::Usage, "OPC-UA node_id is invalid")
                .details(json!({"node_index":index}))
        })?;
        Ok(request(node_id))
    }).collect()
}
fn invalid(message: &'static str) -> anyhow::Error {
    Categorized::error(ErrorKind::Incomplete, message)
}
fn check(deadline: Instant) -> Result<()> {
    if Instant::now() >= deadline {
        return Err(timeout_error());
    }
    Ok(())
}

pub(super) fn resolve(
    nodes: &[Node],
    values: &[DataValue],
    deadline: Instant,
) -> Result<Vec<ReadValueId>> {
    check(deadline)?;
    let [value] = values else {
        return Err(invalid(
            "OPC-UA NamespaceArray Read returned an unexpected node count",
        ));
    };
    if !value.status().is_good() {
        return Err(Categorized::new(
            ErrorKind::Unavailable,
            "OPC-UA NamespaceArray returned a non-Good status",
        )
        .details(json!({"opcua_status":value.status().bits()}))
        .into());
    }
    let Some(Variant::Array(array)) = &value.value else {
        return Err(invalid("OPC-UA NamespaceArray must be a String array"));
    };
    if array.value_type != VariantScalarTypeId::String
        || array.values.is_empty() || array.values.len() > MAX_NAMESPACES
        || array.dimensions.as_ref().is_some_and(|dimensions| {
            !matches!(dimensions.as_slice(), [length] if usize::try_from(*length).ok() == Some(array.values.len()))
        })
    {
        return Err(invalid("OPC-UA NamespaceArray must contain 1–65536 one-dimensional String entries"));
    }
    let mut namespaces = BTreeMap::new();
    let mut bytes = 4_usize;
    for (index, value) in array.values.iter().enumerate() {
        check(deadline)?;
        let Variant::String(value) = value else {
            return Err(invalid("OPC-UA NamespaceArray contains a non-String entry"));
        };
        let Some(uri) = value.value().as_deref() else {
            return Err(invalid("OPC-UA NamespaceArray contains a null URI"));
        };
        if !valid_uri(uri) {
            return Err(Categorized::new(
                ErrorKind::Incomplete,
                "OPC-UA NamespaceArray contains an empty, oversized or invalid URI",
            )
            .details(json!({"namespace_index":index}))
            .into());
        }
        if index == 0 && uri != STANDARD {
            return Err(invalid(
                "OPC-UA NamespaceArray index zero is not the standard namespace URI",
            ));
        }
        let Some(total) = bytes
            .checked_add(4 + uri.len())
            .filter(|total| *total <= MAX_MESSAGE)
        else {
            return Err(invalid("OPC-UA NamespaceArray exceeds the 1 MiB limit"));
        };
        bytes = total;
        let index =
            u16::try_from(index).map_err(|_| invalid("OPC-UA namespace index exceeds UInt16"))?;
        if namespaces.insert(uri, index).is_some() {
            return Err(Categorized::new(
                ErrorKind::Incomplete,
                "OPC-UA NamespaceArray contains a duplicate URI",
            )
            .details(json!({"namespace_index":index}))
            .into());
        }
    }
    let mut requests = prepare(nodes)?;
    for (index, (node, request)) in nodes.iter().zip(&mut requests).enumerate() {
        check(deadline)?;
        if let Some(uri) = &node.namespace_uri {
            let Some(namespace) = namespaces.get(uri.as_str()) else {
                return Err(Categorized::new(
                    ErrorKind::Unavailable,
                    "OPC-UA namespace_uri is absent from this session's NamespaceArray",
                )
                .details(json!({"node_index":index}))
                .into());
            };
            request.node_id.namespace = *namespace;
        }
    }
    check(deadline)?;
    Ok(requests)
}

#[cfg(test)]
mod tests {
    use super::*;
    use opcua::types::{Array, StatusCode, UAString};
    use std::time::Duration;

    fn node(id: &str, uri: Option<&str>) -> Node {
        Node {
            name: id.into(),
            node_id: id.into(),
            namespace_uri: uri.map(str::to_owned),
        }
    }
    fn table(uris: &[&str]) -> DataValue {
        DataValue::value_only(
            uris.iter()
                .map(|uri| UAString::from(*uri))
                .collect::<Vec<_>>(),
        )
    }
    fn resolve_table(nodes: &[Node], values: &[DataValue]) -> Result<Vec<ReadValueId>> {
        resolve(nodes, values, Instant::now() + Duration::from_secs(5))
    }
    #[test]
    fn preserves_all_identifier_kinds_and_legacy_indices_with_exact_uri_matching() {
        let uri = "urn:BeckhoffAutomation:Ua:PLC1";
        let nodes = [
            node("s=GVL.Pressure", Some(uri)),
            node("i=4294967295", Some(uri)),
            node("g=09087e75-8e5e-499b-954f-f2a9603db28a", Some(uri)),
            node("b=AP+A", Some(uri)),
            node("ns=7;s=legacy", None),
            node("i=2259", Some(STANDARD)),
        ];
        let first = resolve_table(&nodes, &[table(&[STANDARD, "urn:server", uri])]).unwrap();
        let second = resolve_table(
            &nodes,
            &[table(&[STANDARD, "urn:server", "urn:padding", uri])],
        )
        .unwrap();
        assert_eq!(first.len(), nodes.len());
        for ((first, second), configured) in first.iter().zip(&second).zip(&nodes) {
            let parsed = configured.node_id.parse::<NodeId>().unwrap();
            assert_eq!(first.node_id.identifier, parsed.identifier);
            assert_eq!(second.node_id.identifier, parsed.identifier);
            let expected = if configured.namespace_uri.as_deref() == Some(uri) {
                (2, 3)
            } else {
                (parsed.namespace, parsed.namespace)
            };
            assert_eq!(
                (first.node_id.namespace, second.node_id.namespace),
                expected
            );
        }
        resolve_table(
            &[node(
                "s=GVL.Pressure",
                Some("urn:beckhoffAutomation:Ua:PLC1"),
            )],
            &[table(&[STANDARD, "urn:server", uri])],
        )
        .unwrap_err();
        let encoded = serde_json::to_value(node("ns=2;s=P", None)).unwrap();
        assert!(encoded.get("namespace_uri").is_none());
    }
    #[test]
    fn rejects_namespace_prefix_conflicts_and_invalid_uri_configuration_before_connecting() {
        for id in [
            "ns=0;s=P",
            "ns=2;s=P",
            "nsu=urn:x;s=P",
            "svr=1;s=P",
            "svu=urn:x;s=P",
        ] {
            let error = prepare(&[node(id, Some("urn:x"))]).unwrap_err();
            assert_eq!(crate::types::error_transport_code(&error), Some("usage"));
        }
        for uri in ["", " urn:x", "urn:x\n", "urn:x\t"] {
            prepare(&[node("s=P", Some(uri))]).unwrap_err();
        }
        prepare(&[node("s=P", Some(&"x".repeat(MAX_URI + 1)))]).unwrap_err();
        prepare(&[node("i=4294967296", Some("urn:x"))]).unwrap_err();
        prepare(&[node("g=wrong", Some("urn:x"))]).unwrap_err();
        prepare(&[node("b=???", Some("urn:x"))]).unwrap_err();
    }
    #[test]
    fn malformed_namespace_reads_fail_atomically_without_index_fallback() {
        let nodes = [node("ns=7;s=legacy", None), node("s=P", Some("urn:wanted"))];
        let good = table(&[STANDARD, "urn:server", "urn:wanted"]);
        let bad_status =
            DataValue::from((good.value.clone().unwrap(), StatusCode::BadUserAccessDenied));
        let faults = [
            Vec::new(),
            vec![good.clone(), good.clone()],
            vec![DataValue::null()],
            vec![DataValue::value_only(42_u32)],
            vec![DataValue::value_only(vec![1_u32, 2])],
            vec![bad_status],
            vec![table(&[])],
            vec![table(&["urn:wrong-zero", "urn:wanted"])],
            vec![table(&[STANDARD, ""])],
            vec![table(&[STANDARD, "urn:wanted", "urn:wanted"])],
            vec![table(&[STANDARD, "urn:other"])],
            vec![DataValue::value_only(vec![
                UAString::from(STANDARD),
                UAString::null(),
            ])],
            vec![DataValue::value_only(Variant::Array(Box::new(Array {
                value_type: VariantScalarTypeId::String,
                values: vec![Variant::String(STANDARD.into()), Variant::UInt32(2)],
                dimensions: None,
            })))],
            vec![DataValue::value_only(Variant::Array(Box::new(Array {
                value_type: VariantScalarTypeId::String,
                values: vec![
                    Variant::String(STANDARD.into()),
                    Variant::String("urn:wanted".into()),
                ],
                dimensions: Some(vec![1, 2]),
            })))],
        ];
        for values in faults {
            resolve_table(&nodes, &values).unwrap_err();
        }
        assert_eq!(resolve_table(&nodes, &[good]).unwrap().len(), 2);
    }
    #[test]
    fn namespace_table_count_byte_budget_and_deadline_are_bounded() {
        let nodes = [node("s=P", Some("urn:wanted"))];
        let entries = std::iter::once(UAString::from(STANDARD))
            .chain((1..=MAX_NAMESPACES).map(|index| UAString::from(format!("urn:{index}"))))
            .collect::<Vec<_>>();
        resolve_table(&nodes, &[DataValue::value_only(entries)]).unwrap_err();
        let entries = std::iter::once(UAString::from(STANDARD))
            .chain(
                (1..270).map(|index| UAString::from(format!("urn:{index}:{}", "x".repeat(4030)))),
            )
            .collect::<Vec<_>>();
        let error = resolve_table(&nodes, &[DataValue::value_only(entries)]).unwrap_err();
        assert!(format!("{error:#}").contains("1 MiB"));
        let error =
            resolve(&nodes, &[table(&[STANDARD, "urn:wanted"])], Instant::now()).unwrap_err();
        assert_eq!(crate::types::error_transport_code(&error), Some("timeout"));
    }
}
