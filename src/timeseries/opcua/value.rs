//! Preserve OPC-UA values in JSON without rounding integers or flattening matrices.
use crate::types::{Categorized, ErrorKind};
use anyhow::Result;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use chrono::SecondsFormat;
use opcua::types::{
    Argument, Array, ByteString, ComplexNumberType, ContextOwned, DataValue, DecodingOptions,
    DiagnosticInfo, DoubleComplexNumberType, EUInformation, EnumValueType, ExtensionObject,
    LocalizedText, NamespaceMap, Range, UAString, UaNullable as _, Variant, XVType,
    type_loader::{ByteStringBody, JsonBody, XmlBody},
};
use serde_json::{Value, json};
use std::io::{self, Write};

const MAX_DEPTH: usize = 32;
const MAX_BODY_BYTES: usize = 1024 * 1024;

pub(super) fn convert(value: &Variant) -> Result<Value> {
    convert_at(value, 0)
}

fn check_depth(depth: usize) -> Result<()> {
    if depth > MAX_DEPTH {
        return Err(Categorized::error(
            ErrorKind::Incomplete,
            "OPC-UA value exceeds the nesting limit of 32",
        ));
    }
    Ok(())
}

fn unsupported(message: &str) -> anyhow::Error {
    Categorized::error(ErrorKind::Unsupported, message)
}

fn convert_at(value: &Variant, depth: usize) -> Result<Value> {
    check_depth(depth)?;
    Ok(match value {
        Variant::Empty => Value::Null,
        Variant::Boolean(value) => (*value).into(),
        Variant::SByte(value) => (*value).into(),
        Variant::Byte(value) => (*value).into(),
        Variant::Int16(value) => (*value).into(),
        Variant::UInt16(value) => (*value).into(),
        Variant::Int32(value) => (*value).into(),
        Variant::UInt32(value) => (*value).into(),
        Variant::Int64(value) => (*value).into(),
        Variant::UInt64(value) => (*value).into(),
        Variant::Float(value) => finite(f64::from(*value))?,
        Variant::Double(value) => finite(*value)?,
        Variant::String(value) => string(value),
        Variant::DateTime(value) => timestamp(value),
        Variant::Guid(value) => json!({"type": "guid", "value": value.to_string()}),
        Variant::StatusCode(value) => tagged("StatusCode", json!({"Value": value.bits()})),
        Variant::ByteString(value) => bytes(value),
        Variant::XmlElement(value) => json!({
            "type": "XmlElement",
            "value": if value.is_ua_null() { Value::Null } else { value.to_string().into() },
        }),
        Variant::QualifiedName(value) => tagged(
            "QualifiedName",
            json!({"NamespaceIndex": value.namespace_index, "Name": string(&value.name)}),
        ),
        Variant::LocalizedText(value) => localized_text(value),
        Variant::NodeId(value) => json!({"type": "NodeId", "value": value.to_string()}),
        Variant::ExpandedNodeId(value) => {
            json!({"type": "ExpandedNodeId", "value": value.to_string()})
        }
        Variant::ExtensionObject(value) => extension_object(value, depth + 1)?,
        Variant::Variant(value) => convert_at(value, depth + 1)?,
        Variant::DataValue(value) => data_value(value, depth + 1)?,
        Variant::DiagnosticInfo(value) => diagnostic_info(value, depth + 1)?,
        Variant::Array(value) => array(value, depth + 1)?,
    })
}

fn finite(value: f64) -> Result<Value> {
    serde_json::Number::from_f64(value)
        .map(Value::Number)
        .ok_or_else(|| unsupported("OPC-UA value contains a non-finite floating-point number"))
}

fn string(value: &UAString) -> Value {
    value
        .value()
        .as_ref()
        .map_or(Value::Null, |s| s.clone().into())
}

fn timestamp(value: &opcua::types::DateTime) -> Value {
    // The SDK's to_rfc3339 method forces millisecond precision.
    value
        .as_chrono()
        .to_rfc3339_opts(SecondsFormat::AutoSi, true)
        .into()
}

fn bytes(value: &ByteString) -> Value {
    value
        .value
        .as_ref()
        .map_or(Value::Null, |value| base64_bytes(value))
}

fn base64_bytes(value: &[u8]) -> Value {
    json!({"encoding": "base64", "data": STANDARD.encode(value)})
}

fn tagged(name: &str, fields: Value) -> Value {
    Value::Object(
        [("type".into(), name.into()), ("fields".into(), fields)]
            .into_iter()
            .collect(),
    )
}

fn localized_text(value: &LocalizedText) -> Value {
    tagged(
        "LocalizedText",
        json!({"Locale": string(&value.locale), "Text": string(&value.text)}),
    )
}

fn array(value: &Array, depth: usize) -> Result<Value> {
    check_depth(depth)?;
    // The SDK decodes ordinary zero-length arrays with an empty dimensions
    // vector even when the wire value has no matrix dimensions.
    if value.values.is_empty() && value.dimensions.as_ref().is_some_and(Vec::is_empty) {
        return Ok(Value::Array(Vec::new()));
    }
    if let Some(dimensions) = &value.dimensions {
        let length = dimensions.iter().try_fold(1usize, |length, dimension| {
            length.checked_mul(*dimension as usize)
        });
        if dimensions.is_empty() || length != Some(value.values.len()) {
            return Err(unsupported(
                "OPC-UA matrix dimensions do not match its values",
            ));
        }
    }
    let values = value
        .values
        .iter()
        .map(|value| convert_at(value, depth))
        .collect::<Result<Vec<_>>>()?;
    Ok(match &value.dimensions {
        Some(dimensions) => json!({
            "type": "Matrix",
            "element_type": format!("{:?}", value.value_type),
            "dimensions": dimensions,
            "values": values,
        }),
        None => values.into(),
    })
}

fn data_value(value: &DataValue, depth: usize) -> Result<Value> {
    check_depth(depth)?;
    Ok(tagged(
        "DataValue",
        json!({
            "Value": value.value.as_ref().map(|v| convert_at(v, depth)).transpose()?,
            "StatusCode": value.status.map(|status| tagged("StatusCode", json!({"Value": status.bits()}))),
            "SourceTimestamp": value.source_timestamp.as_ref().map(timestamp),
            "SourcePicoseconds": value.source_picoseconds,
            "ServerTimestamp": value.server_timestamp.as_ref().map(timestamp),
            "ServerPicoseconds": value.server_picoseconds,
        }),
    ))
}

fn diagnostic_info(value: &DiagnosticInfo, depth: usize) -> Result<Value> {
    check_depth(depth)?;
    Ok(tagged(
        "DiagnosticInfo",
        json!({
            "SymbolicId": value.symbolic_id,
            "NamespaceUri": value.namespace_uri,
            "Locale": value.locale,
            "LocalizedText": value.localized_text,
            "AdditionalInfo": value.additional_info.as_ref().map(string),
            "InnerStatusCode": value.inner_status_code.map(|status| tagged("StatusCode", json!({"Value": status.bits()}))),
            "InnerDiagnosticInfo": value.inner_diagnostic_info.as_ref().map(|v| diagnostic_info(v, depth + 1)).transpose()?,
        }),
    ))
}

fn extension_object(value: &ExtensionObject, depth: usize) -> Result<Value> {
    check_depth(depth)?;
    let Some(body) = &value.body else {
        return Ok(json!({"type": "ExtensionObject", "body": null}));
    };
    if let Some(value) = value.inner_as::<Range>() {
        return Ok(tagged(
            "Range",
            json!({"Low": finite(value.low)?, "High": finite(value.high)?}),
        ));
    }
    if let Some(value) = value.inner_as::<EUInformation>() {
        return Ok(tagged(
            "EUInformation",
            json!({
                "NamespaceUri": string(&value.namespace_uri), "UnitId": value.unit_id,
                "DisplayName": localized_text(&value.display_name),
                "Description": localized_text(&value.description),
            }),
        ));
    }
    if let Some(value) = value.inner_as::<Argument>() {
        return Ok(tagged(
            "Argument",
            json!({
                "Name": string(&value.name),
                "DataType": {"type": "NodeId", "value": value.data_type.to_string()},
                "ValueRank": value.value_rank, "ArrayDimensions": value.array_dimensions,
                "Description": localized_text(&value.description),
            }),
        ));
    }
    if let Some(value) = value.inner_as::<EnumValueType>() {
        return Ok(tagged(
            "EnumValueType",
            json!({
                "Value": value.value, "DisplayName": localized_text(&value.display_name),
                "Description": localized_text(&value.description),
            }),
        ));
    }
    if let Some(value) = value.inner_as::<ComplexNumberType>() {
        return Ok(tagged(
            "ComplexNumberType",
            json!({
                "Real": finite(f64::from(value.real))?, "Imaginary": finite(f64::from(value.imaginary))?,
            }),
        ));
    }
    if let Some(value) = value.inner_as::<DoubleComplexNumberType>() {
        return Ok(tagged(
            "DoubleComplexNumberType",
            json!({
                "Real": finite(value.real)?, "Imaginary": finite(value.imaginary)?,
            }),
        ));
    }
    if let Some(value) = value.inner_as::<XVType>() {
        return Ok(tagged(
            "XVType",
            json!({"X": finite(value.x)?, "Value": finite(f64::from(value.value))?}),
        ));
    }
    let (encoding, encoding_id, raw) = if let Some(value) = value.inner_as::<ByteStringBody>() {
        (
            "binary",
            value.encoding_id().to_string(),
            bytes(value.raw_body()),
        )
    } else if let Some(value) = value.inner_as::<XmlBody>() {
        (
            "xml",
            value.encoding_id().to_string(),
            base64_bytes(value.raw_body()),
        )
    } else if let Some(value) = value.inner_as::<JsonBody>() {
        (
            "json",
            value.encoding_id().to_string(),
            base64_bytes(value.raw_body()),
        )
    } else {
        // Keep unrecognized standard/vendor structures losslessly as OPC-UA binary.
        // The transport already bounds their decoded nesting and message size.
        let context = ContextOwned::new_default(NamespaceMap::new(), DecodingOptions::default());
        let mut output = BoundedBody::default();
        let result = body.encode_binary(&mut output, &context.context());
        if output.exceeded {
            return Err(Categorized::error(
                ErrorKind::Incomplete,
                "OPC-UA extension body exceeds 1 MiB",
            ));
        }
        result.map_err(|_| unsupported("OPC-UA extension body cannot be preserved as binary"))?;
        (
            "binary",
            body.binary_type_id().to_string(),
            base64_bytes(&output.bytes),
        )
    };
    Ok(json!({
        "type": "ExtensionObject", "encoding": encoding,
        "encoding_type_id": encoding_id, "body": raw,
    }))
}

#[derive(Default)]
struct BoundedBody {
    bytes: Vec<u8>,
    exceeded: bool,
}

impl Write for BoundedBody {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_BODY_BYTES.saturating_sub(self.bytes.len()) {
            self.exceeded = true;
            return Err(io::Error::other("OPC-UA extension body exceeds 1 MiB"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opcua::types::{
        BinaryDecodable as _, BinaryEncodable as _, DateTime, ExpandedNodeId, Guid, NodeId,
        StatusCode, VariantScalarTypeId,
    };
    use std::{io::Cursor, str::FromStr as _};

    #[test]
    fn scalars_preserve_integer_extremes_and_nulls() {
        for (input, expected) in [
            (Variant::Int64(i64::MIN), json!(i64::MIN)),
            (Variant::UInt64(u64::MAX), json!(u64::MAX)),
            (Variant::Boolean(true), json!(true)),
            (Variant::String("text".into()), json!("text")),
            (Variant::String(UAString::null()), Value::Null),
            (Variant::Empty, Value::Null),
        ] {
            let converted = convert(&input).unwrap();
            assert_eq!(converted, expected);
            assert_eq!(
                serde_json::from_str::<Value>(&converted.to_string()).unwrap(),
                expected
            );
        }
    }

    #[test]
    fn nonfinite_values_are_unsupported_even_in_arrays_or_known_structures() {
        for input in [
            Variant::Double(f64::NAN),
            Variant::Float(f32::INFINITY),
            Variant::Double(f64::NEG_INFINITY),
            Variant::Array(Box::new(
                Array::new(VariantScalarTypeId::Double, vec![Variant::Double(f64::NAN)]).unwrap(),
            )),
            Variant::ExtensionObject(ExtensionObject::new(Range {
                low: f64::NAN,
                high: 1.0,
            })),
        ] {
            assert_eq!(
                convert(&input)
                    .unwrap_err()
                    .downcast_ref::<Categorized>()
                    .unwrap()
                    .kind,
                ErrorKind::Unsupported
            );
        }
    }

    #[test]
    fn bytes_keep_null_distinct_from_empty() {
        assert_eq!(
            convert(&Variant::ByteString(ByteString::null())).unwrap(),
            Value::Null
        );
        assert_eq!(
            convert(&Variant::ByteString(Vec::<u8>::new().into())).unwrap(),
            json!({"encoding": "base64", "data": ""})
        );
        assert_eq!(
            convert(&Variant::ByteString(vec![0, 255, 1].into())).unwrap(),
            json!({"encoding": "base64", "data": "AP8B"})
        );
    }

    #[test]
    fn timestamps_retain_submillisecond_precision_and_utc() {
        let value = DateTime::parse_from_rfc3339("2026-01-02T03:04:05.123456700+02:00").unwrap();
        assert_eq!(
            convert(&Variant::DateTime(Box::new(value))).unwrap(),
            json!("2026-01-02T01:04:05.123456700Z")
        );
    }

    #[test]
    fn identifiers_have_explicit_types() {
        let id = NodeId::new(2, "temperature");
        assert_eq!(
            convert(&Variant::NodeId(Box::new(id.clone()))).unwrap(),
            json!({"type": "NodeId", "value": id.to_string()})
        );
        let expanded = ExpandedNodeId {
            node_id: id,
            namespace_uri: "urn:plant".into(),
            server_index: 4,
        };
        assert_eq!(
            convert(&Variant::ExpandedNodeId(Box::new(expanded.clone()))).unwrap(),
            json!({"type": "ExpandedNodeId", "value": expanded.to_string()})
        );
        let guid = Guid::from_str("01234567-89ab-cdef-0123-456789abcdef").unwrap();
        assert_eq!(
            convert(&Variant::Guid(Box::new(guid))).unwrap(),
            json!({"type": "guid", "value": "01234567-89ab-cdef-0123-456789abcdef"})
        );
    }

    #[test]
    fn matrices_keep_dimensions_and_flat_order() {
        let values = (1..=6).map(Variant::Int32).collect::<Vec<_>>();
        let matrix =
            Array::new_multi(VariantScalarTypeId::Int32, values.clone(), vec![2, 3]).unwrap();
        assert_eq!(
            convert(&Variant::Array(Box::new(matrix))).unwrap(),
            json!({"type":"Matrix", "element_type":"Int32", "dimensions":[2,3], "values":[1,2,3,4,5,6]})
        );
        assert_eq!(
            convert(&Variant::Array(Box::new(
                Array::new(VariantScalarTypeId::Int32, values).unwrap()
            )))
            .unwrap(),
            json!([1, 2, 3, 4, 5, 6])
        );
        let invalid = Array {
            value_type: VariantScalarTypeId::Int32,
            values: vec![1i32.into()],
            dimensions: Some(vec![u32::MAX, u32::MAX, u32::MAX]),
        };
        assert_eq!(
            convert(&Variant::Array(Box::new(invalid)))
                .unwrap_err()
                .downcast_ref::<Categorized>()
                .unwrap()
                .kind,
            ErrorKind::Unsupported
        );
    }

    #[test]
    fn binary_empty_array_scalar_and_matrix_preserve_stream_alignment_and_values() {
        let inputs = [
            Variant::Array(Box::new(
                Array::new(VariantScalarTypeId::UInt32, Vec::new()).unwrap(),
            )),
            Variant::UInt64(u64::MAX),
            Variant::Array(Box::new(
                Array::new_multi(
                    VariantScalarTypeId::Int32,
                    (1..=6).map(Variant::Int32).collect::<Vec<_>>(),
                    vec![2, 3],
                )
                .unwrap(),
            )),
        ];
        let context = ContextOwned::new_default(NamespaceMap::new(), DecodingOptions::default());
        let mut wire = Vec::new();
        for input in &inputs {
            input.encode(&mut wire, &context.context()).unwrap();
        }
        let mut cursor = Cursor::new(&wire);
        for expected in [
            json!([]),
            json!(u64::MAX),
            json!({"type":"Matrix", "element_type":"Int32", "dimensions":[2,3], "values":[1,2,3,4,5,6]}),
        ] {
            let decoded = Variant::decode(&mut cursor, &context.context()).unwrap();
            assert_eq!(convert(&decoded).unwrap(), expected);
        }
        assert_eq!(cursor.position(), wire.len() as u64);
    }

    #[test]
    fn data_values_keep_status_timestamps_and_picoseconds() {
        let value = DataValue {
            value: Some(Variant::UInt64(u64::MAX)),
            status: Some(StatusCode::from(0x4000_0000)),
            source_timestamp: Some(DateTime::parse_from_rfc3339("2026-01-02T00:00:00Z").unwrap()),
            source_picoseconds: Some(99),
            server_timestamp: None,
            server_picoseconds: None,
        };
        let converted = convert(&Variant::DataValue(Box::new(value))).unwrap();
        assert_eq!(converted["fields"]["Value"], json!(u64::MAX));
        assert_eq!(
            converted["fields"]["StatusCode"]["fields"]["Value"],
            json!(0x4000_0000u32)
        );
        assert_eq!(
            converted["fields"]["SourceTimestamp"],
            json!("2026-01-02T00:00:00Z")
        );
        assert_eq!(converted["fields"]["SourcePicoseconds"], json!(99));
    }

    #[test]
    fn nested_variant_and_diagnostics_share_the_depth_limit() {
        let mut value = Variant::Int32(1);
        for _ in 0..MAX_DEPTH {
            value = Variant::Variant(Box::new(value));
        }
        assert_eq!(convert(&value).unwrap(), json!(1));
        value = Variant::Variant(Box::new(value));
        assert_eq!(
            convert(&value)
                .unwrap_err()
                .downcast_ref::<Categorized>()
                .unwrap()
                .kind,
            ErrorKind::Incomplete
        );
        let mut diagnostic = DiagnosticInfo::default();
        for _ in 0..MAX_DEPTH {
            diagnostic = DiagnosticInfo {
                inner_diagnostic_info: Some(Box::new(diagnostic)),
                ..Default::default()
            };
        }
        assert_eq!(
            convert(&Variant::DiagnosticInfo(Box::new(diagnostic)))
                .unwrap_err()
                .downcast_ref::<Categorized>()
                .unwrap()
                .kind,
            ErrorKind::Incomplete
        );
    }

    #[test]
    fn opaque_extension_objects_retain_encoding_identifier_and_exact_bytes() {
        let id = NodeId::new(3, 1001u32);
        for (body, encoding) in [
            (
                ExtensionObject::new(ByteStringBody::new(vec![0, 255, 1].into(), id.clone())),
                "binary",
            ),
            (
                ExtensionObject::new(XmlBody::new(vec![0, 255, 1], id.clone(), "Example".into())),
                "xml",
            ),
            (
                ExtensionObject::new(JsonBody::new(vec![0, 255, 1], id.clone())),
                "json",
            ),
        ] {
            assert_eq!(
                convert(&Variant::ExtensionObject(body)).unwrap(),
                json!({"type":"ExtensionObject", "encoding":encoding, "encoding_type_id":id.to_string(), "body":{"encoding":"base64", "data":"AP8B"}})
            );
        }
    }

    #[test]
    fn unrecognized_decoded_extension_objects_are_preserved_as_binary() {
        let value = opcua::types::TimeZoneDataType {
            offset: -120,
            daylight_saving_in_offset: true,
        };
        let body = ExtensionObject::new(value);
        let id = body.binary_type_id().to_string();
        assert_eq!(
            convert(&Variant::ExtensionObject(body)).unwrap(),
            json!({"type":"ExtensionObject", "encoding":"binary", "encoding_type_id":id, "body":{"encoding":"base64", "data":"iP8B"}})
        );
    }

    #[test]
    fn decoded_extension_body_writer_is_bounded() {
        let mut writer = BoundedBody::default();
        writer.write_all(&vec![0; MAX_BODY_BYTES]).unwrap();
        assert!(writer.write_all(&[1]).is_err());
        assert!(writer.exceeded);
        assert_eq!(writer.bytes.len(), MAX_BODY_BYTES);
    }
}
