//! Bounded, typed SDK-decoded data; never an original-wire or UTC-clock claim.
use crate::types::{Categorized, ErrorKind};
use anyhow::Result;
use base64::{Engine as _, display::Base64Display, engine::general_purpose::STANDARD};
use opcua::types::{
    Array, ByteString, ContextOwned, DataValue, DecodingOptions, DiagnosticInfo, DynEncodable,
    ExpandedNodeId, ExtensionObject, Identifier, NamespaceMap, NodeId, UAString, UaNullable as _,
    Variant, VariantScalarTypeId, VariantTypeId,
    type_loader::{ByteStringBody, JsonBody, XmlBody},
};
use serde::{Serialize, Serializer};
use serde_json::{Map, Value};
use std::{
    fmt,
    io::{self, Write},
    time::Instant,
};

const MAX_JSON: usize = (1 << 20) - 16 * 1024;
const MAX_BINARY: usize = 1 << 20;
const MAX_DEPTH: usize = 32;
const MAX_VISITS: usize = 2 << 20;

fn invalid(message: &'static str) -> anyhow::Error {
    Categorized::error(ErrorKind::Incomplete, message)
}
fn check_time(deadline: Instant) -> Result<()> {
    if Instant::now() >= deadline {
        Err(Categorized::error(
            ErrorKind::Timeout,
            "OPC-UA native value deadline expired",
        ))
    } else {
        Ok(())
    }
}

struct Count {
    bytes: usize,
    limit: usize,
    deadline: Instant,
}
impl Write for Count {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if Instant::now() >= self.deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "native encoding deadline",
            ));
        }
        self.bytes = self.bytes.saturating_add(bytes.len());
        if self.bytes > self.limit {
            return Err(io::Error::other("native encoding bound"));
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
struct Collect {
    count: Count,
    bytes: Vec<u8>,
}
impl Write for Collect {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.count.write_all(bytes)?;
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
struct Text<'a, T: ?Sized>(&'a T);
impl<T: fmt::Display + ?Sized> Serialize for Text<'_, T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.collect_str(self.0)
    }
}
struct Base64<'a>(&'a [u8]);
impl Serialize for Base64<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.collect_str(&Base64Display::new(self.0, &STANDARD))
    }
}

struct Budget {
    remaining: usize,
    visits: usize,
    deadline: Instant,
}
impl Budget {
    fn new(deadline: Instant) -> Self {
        Self {
            remaining: MAX_JSON,
            visits: 0,
            deadline,
        }
    }
    fn visit(&mut self, depth: usize) -> Result<()> {
        check_time(self.deadline)?;
        self.visits = self.visits.saturating_add(1);
        if depth > MAX_DEPTH || self.visits > MAX_VISITS {
            return Err(invalid("OPC-UA native value exceeds depth or work bound"));
        }
        Ok(())
    }
    fn charge(&mut self, bytes: usize) -> Result<()> {
        check_time(self.deadline)?;
        self.remaining = self
            .remaining
            .checked_sub(bytes)
            .ok_or_else(|| invalid("OPC-UA native value exceeds JSON admission bound"))?;
        Ok(())
    }
    fn object(&mut self) -> Result<Map<String, Value>> {
        self.charge(2)?;
        Ok(Map::new())
    }
    fn key(&mut self, object: &Map<String, Value>, key: &'static str) -> Result<()> {
        self.charge(key.len() + 3 + usize::from(!object.is_empty()))
    }
    fn add(
        &mut self,
        object: &mut Map<String, Value>,
        key: &'static str,
        value: impl FnOnce(&mut Self) -> Result<Value>,
    ) -> Result<()> {
        self.key(object, key)?;
        let value = value(self)?;
        object.insert(key.into(), value);
        Ok(())
    }
    fn scalar<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<Value> {
        self.visit(0)?;
        let mut count = Count {
            bytes: 0,
            limit: self.remaining,
            deadline: self.deadline,
        };
        serde_json::to_writer(&mut count, value).map_err(|_| {
            check_time(self.deadline)
                .err()
                .unwrap_or_else(|| invalid("OPC-UA native scalar exceeds JSON admission bound"))
        })?;
        self.charge(count.bytes)?;
        let value = serde_json::to_value(value)?;
        check_time(self.deadline)?;
        Ok(value)
    }
    fn field<T: Serialize + ?Sized>(
        &mut self,
        object: &mut Map<String, Value>,
        key: &'static str,
        value: &T,
    ) -> Result<()> {
        self.add(object, key, |budget| budget.scalar(value))
    }
    fn typed(&mut self, name: &'static str, dense: bool) -> Result<Map<String, Value>> {
        let mut object = self.object()?;
        if !dense {
            self.field(&mut object, "type", name)?;
        }
        Ok(object)
    }
    fn simple<T: Serialize + ?Sized>(
        &mut self,
        name: &'static str,
        value: &T,
        dense: bool,
    ) -> Result<Value> {
        if dense {
            return self.scalar(value);
        }
        let mut object = self.typed(name, false)?;
        self.field(&mut object, "value", value)?;
        Ok(Value::Object(object))
    }
    fn nullable_text(
        &mut self,
        name: &'static str,
        value: &UAString,
        dense: bool,
    ) -> Result<Value> {
        let mut object = self.typed(name, dense)?;
        self.field(&mut object, "is_null", &value.value().is_none())?;
        if let Some(value) = value.value() {
            self.field(&mut object, "value", value)?;
        }
        Ok(Value::Object(object))
    }
    fn nullable_bytes(&mut self, value: &ByteString, dense: bool) -> Result<Value> {
        let mut object = self.typed("ByteString", dense)?;
        self.field(&mut object, "is_null", &value.value.is_none())?;
        if let Some(value) = &value.value {
            self.field(&mut object, "base64", &Base64(value))?;
        }
        Ok(Value::Object(object))
    }
    fn float(
        &mut self,
        name: &'static str,
        bits: u64,
        sign_bit: bool,
        value: f64,
        dense: bool,
    ) -> Result<Value> {
        let mut object = self.typed(name, dense)?;
        self.field(&mut object, "bits", &bits)?;
        let classification = if value.is_nan() {
            "nan"
        } else if value == f64::INFINITY {
            "positive_infinity"
        } else if value == f64::NEG_INFINITY {
            "negative_infinity"
        } else {
            "finite"
        };
        self.field(&mut object, "classification", classification)?;
        self.field(&mut object, "sign_bit", &sign_bit)?;
        if value.is_finite() {
            self.field(&mut object, "value", &value)?;
        }
        Ok(Value::Object(object))
    }
    fn node_id(&mut self, value: &NodeId, depth: usize) -> Result<Value> {
        self.visit(depth)?;
        let mut object = self.typed("NodeId", false)?;
        self.field(&mut object, "namespace", &value.namespace)?;
        let name = match &value.identifier {
            Identifier::Numeric(_) => "Numeric",
            Identifier::String(_) => "String",
            Identifier::Guid(_) => "Guid",
            Identifier::ByteString(_) => "ByteString",
        };
        self.field(&mut object, "identifier_type", name)?;
        self.add(&mut object, "identifier", |budget| {
            match &value.identifier {
                Identifier::Numeric(value) => budget.scalar(value),
                Identifier::String(value) => budget.nullable_text("String", value, true),
                Identifier::Guid(value) => budget.scalar(&Text(value)),
                Identifier::ByteString(value) => budget.nullable_bytes(value, true),
            }
        })?;
        Ok(Value::Object(object))
    }
    fn expanded_id(&mut self, value: &ExpandedNodeId, depth: usize) -> Result<Value> {
        self.visit(depth)?;
        let mut object = self.typed("ExpandedNodeId", false)?;
        self.add(&mut object, "node_id", |budget| {
            budget.node_id(&value.node_id, depth + 1)
        })?;
        self.add(&mut object, "namespace_uri", |budget| {
            budget.nullable_text("String", &value.namespace_uri, true)
        })?;
        self.field(&mut object, "server_index", &value.server_index)?;
        Ok(Value::Object(object))
    }
    fn diagnostic(&mut self, value: &DiagnosticInfo, depth: usize) -> Result<Value> {
        self.visit(depth)?;
        let mut object = self.object()?;
        self.field(&mut object, "kind", "opcua_diagnostic_info")?;
        self.field(&mut object, "type", "DiagnosticInfo")?;
        for (key, number) in [
            ("symbolic_id", value.symbolic_id),
            ("namespace_uri", value.namespace_uri),
            ("locale", value.locale),
            ("localized_text", value.localized_text),
        ] {
            if let Some(number) = number {
                self.field(&mut object, key, &number)?;
            }
        }
        if let Some(info) = &value.additional_info {
            self.add(&mut object, "additional_info", |budget| {
                budget.nullable_text("String", info, true)
            })?;
        }
        if let Some(status) = value.inner_status_code {
            self.field(&mut object, "inner_status_raw", &status.bits())?;
        }
        if let Some(inner) = &value.inner_diagnostic_info {
            self.add(&mut object, "inner_diagnostic_info", |budget| {
                budget.diagnostic(inner, depth + 1)
            })?;
        }
        Ok(Value::Object(object))
    }
    fn data(&mut self, value: &DataValue, depth: usize) -> Result<Value> {
        self.visit(depth)?;
        let mut object = self.object()?;
        self.field(&mut object, "kind", "opcua_data_value")?;
        self.field(&mut object, "type", "DataValue")?;
        self.field(&mut object, "representation", "async_opcua_0_19_decoded")?;
        self.field(&mut object, "value_present", &value.value.is_some())?;
        self.field(&mut object, "status_present", &value.status.is_some())?;
        if let Some(status) = value.status {
            self.field(&mut object, "status_raw", &status.bits())?;
        }
        for (key, time) in [
            ("source_ticks", &value.source_timestamp),
            ("server_ticks", &value.server_timestamp),
        ] {
            if let Some(time) = time {
                self.field(&mut object, key, &ticks(time)?)?;
            }
        }
        for (raw, effective, picos, timestamp) in [
            (
                "source_picoseconds_raw",
                "source_picoseconds_effective",
                value.source_picoseconds,
                value.source_timestamp.is_some(),
            ),
            (
                "server_picoseconds_raw",
                "server_picoseconds_effective",
                value.server_picoseconds,
                value.server_timestamp.is_some(),
            ),
        ] {
            if let Some(picos) = picos {
                self.field(&mut object, raw, &picos)?;
                if timestamp {
                    self.field(&mut object, effective, &picos.min(9999))?;
                }
            }
        }
        if let Some(value) = &value.value {
            self.add(&mut object, "value", |budget| {
                budget.variant(value, depth + 1, false)
            })?;
        }
        Ok(Value::Object(object))
    }
    fn array(&mut self, value: &Array, depth: usize) -> Result<Value> {
        self.visit(depth)?;
        if value.values.len() > MAX_VISITS {
            return Err(invalid("OPC-UA native array exceeds work bound"));
        }
        for element in &value.values {
            self.visit(depth)?;
            if !matches!(element.type_id(), VariantTypeId::Scalar(kind) if kind == value.value_type)
            {
                return Err(invalid("OPC-UA native array element types disagree"));
            }
        }
        if let Some(dimensions) = &value.dimensions {
            let product = dimensions
                .iter()
                .try_fold(1usize, |product, item| product.checked_mul(*item as usize));
            if !(value.values.is_empty() && dimensions.is_empty())
                && product != Some(value.values.len())
            {
                return Err(invalid("OPC-UA native matrix dimensions disagree"));
            }
        }
        let mut object = self.typed("Array", false)?;
        self.field(&mut object, "element_type", &Text(&value.value_type))?;
        self.field(&mut object, "null_empty_distinction", "not_retained_by_sdk")?;
        if let Some(dimensions) = &value.dimensions {
            self.field(&mut object, "dimensions", dimensions)?;
        }
        if matches!(
            value.value_type,
            VariantScalarTypeId::Float | VariantScalarTypeId::Double
        ) {
            let width = if value.value_type == VariantScalarTypeId::Float {
                4
            } else {
                8
            };
            let bytes = value
                .values
                .len()
                .checked_mul(width)
                .filter(|bytes| *bytes <= MAX_BINARY)
                .ok_or_else(|| invalid("OPC-UA native float array exceeds binary bound"))?;
            self.field(
                &mut object,
                "element_encoding",
                if width == 4 {
                    "ieee754_le_u32_bits"
                } else {
                    "ieee754_le_u64_bits"
                },
            )?;
            self.field(&mut object, "length", &value.values.len())?;
            self.key(&object, "data_base64")?;
            let encoded = bytes
                .div_ceil(3)
                .checked_mul(4)
                .and_then(|bytes| bytes.checked_add(2))
                .ok_or_else(|| invalid("OPC-UA native base64 size overflow"))?;
            self.charge(encoded)?;
            let mut words = Vec::with_capacity(bytes);
            for element in &value.values {
                self.visit(depth)?;
                match element {
                    Variant::Float(value) => {
                        words.extend_from_slice(&value.to_bits().to_le_bytes());
                    }
                    Variant::Double(value) => {
                        words.extend_from_slice(&value.to_bits().to_le_bytes());
                    }
                    _ => return Err(invalid("OPC-UA native float array type changed")),
                }
            }
            object.insert("data_base64".into(), Value::String(STANDARD.encode(words)));
        } else {
            self.key(&object, "values")?;
            self.charge(2)?;
            let mut values = Vec::new();
            for element in &value.values {
                if !values.is_empty() {
                    self.charge(1)?;
                }
                values.push(self.variant(
                    element,
                    depth + 1,
                    value.value_type != VariantScalarTypeId::Variant,
                )?);
            }
            object.insert("values".into(), Value::Array(values));
        }
        Ok(Value::Object(object))
    }
    fn extension(&mut self, value: &ExtensionObject, depth: usize) -> Result<Value> {
        self.visit(depth)?;
        let mut object = self.typed("ExtensionObject", false)?;
        self.field(&mut object, "body_present", &value.body.is_some())?;
        let Some(body) = &value.body else {
            return Ok(Value::Object(object));
        };
        if let Some(body) = value.inner_as::<ByteStringBody>() {
            self.field(&mut object, "encoding", "binary")?;
            self.add(&mut object, "encoding_type_id", |budget| {
                budget.node_id(body.encoding_id(), depth + 1)
            })?;
            self.add(&mut object, "body", |budget| {
                budget.nullable_bytes(body.raw_body(), true)
            })?;
        } else if let Some(body) = value.inner_as::<XmlBody>() {
            self.field(&mut object, "encoding", "xml")?;
            self.add(&mut object, "encoding_type_id", |budget| {
                budget.node_id(body.encoding_id(), depth + 1)
            })?;
            self.field(&mut object, "body_base64", &Base64(body.raw_body()))?;
        } else if let Some(body) = value.inner_as::<JsonBody>() {
            self.field(&mut object, "encoding", "json")?;
            self.add(&mut object, "encoding_type_id", |budget| {
                budget.node_id(body.encoding_id(), depth + 1)
            })?;
            self.field(&mut object, "body_base64", &Base64(body.raw_body()))?;
        } else {
            self.field(&mut object, "encoding", "ua_binary_sdk_decoded_body")?;
            self.add(&mut object, "encoding_type_id", |budget| {
                budget.expanded_id(&body.binary_type_id(), depth + 1)
            })?;
            self.key(&object, "body_base64")?;
            object.insert("body_base64".into(), self.encoded_body(body.as_ref())?);
        }
        Ok(Value::Object(object))
    }
    fn encoded_body(&mut self, body: &dyn DynEncodable) -> Result<Value> {
        let context = ContextOwned::new_default(NamespaceMap::new(), DecodingOptions::default());
        let mut count = Count {
            bytes: 0,
            limit: MAX_BINARY,
            deadline: self.deadline,
        };
        body.encode_binary(&mut count, &context.context())
            .map_err(|_| {
                check_time(self.deadline).err().unwrap_or_else(|| {
                    invalid("OPC-UA SDK-decoded body exceeds encoding bound or cannot be encoded")
                })
            })?;
        let expected = count.bytes;
        self.charge(
            expected
                .div_ceil(3)
                .checked_mul(4)
                .and_then(|n| n.checked_add(2))
                .ok_or_else(|| invalid("OPC-UA native base64 size overflow"))?,
        )?;
        let mut collected = Collect {
            count: Count {
                bytes: 0,
                limit: expected,
                deadline: self.deadline,
            },
            bytes: Vec::with_capacity(expected),
        };
        body.encode_binary(&mut collected, &context.context())
            .map_err(|_| {
                check_time(self.deadline)
                    .err()
                    .unwrap_or_else(|| invalid("OPC-UA SDK-decoded body changed during encoding"))
            })?;
        if collected.count.bytes != expected {
            return Err(invalid("OPC-UA SDK-decoded body changed size"));
        }
        Ok(Value::String(STANDARD.encode(collected.bytes)))
    }
    fn variant(&mut self, value: &Variant, depth: usize, dense: bool) -> Result<Value> {
        self.visit(depth)?;
        match value {
            Variant::Empty => Ok(Value::Object(self.typed("Null", false)?)),
            Variant::Boolean(v) => self.simple("Boolean", v, dense),
            Variant::SByte(v) => self.simple("SByte", v, dense),
            Variant::Byte(v) => self.simple("Byte", v, dense),
            Variant::Int16(v) => self.simple("Int16", v, dense),
            Variant::UInt16(v) => self.simple("UInt16", v, dense),
            Variant::Int32(v) => self.simple("Int32", v, dense),
            Variant::UInt32(v) => self.simple("UInt32", v, dense),
            Variant::Int64(v) => self.simple("Int64", v, dense),
            Variant::UInt64(v) => self.simple("UInt64", v, dense),
            Variant::Float(v) => self.float(
                "Float",
                u64::from(v.to_bits()),
                v.to_bits() & 0x8000_0000 != 0,
                f64::from(*v),
                dense,
            ),
            Variant::Double(v) => self.float(
                "Double",
                v.to_bits(),
                v.to_bits() & 0x8000_0000_0000_0000 != 0,
                *v,
                dense,
            ),
            Variant::String(v) => self.nullable_text("String", v, dense),
            Variant::ByteString(v) => self.nullable_bytes(v, dense),
            Variant::Guid(v) => self.simple("Guid", &Text(v.as_ref()), dense),
            Variant::StatusCode(v) => self.simple("StatusCode", &v.bits(), dense),
            Variant::DateTime(v) => {
                let mut object = self.typed("DateTime", dense)?;
                self.field(&mut object, "ticks", &ticks(v)?)?;
                Ok(Value::Object(object))
            }
            Variant::XmlElement(v) => {
                let mut object = self.typed("XmlElement", dense)?;
                self.field(&mut object, "is_null", &v.is_ua_null())?;
                if !v.is_ua_null() {
                    self.field(&mut object, "value", &Text(v))?;
                }
                Ok(Value::Object(object))
            }
            Variant::NodeId(v) => self.node_id(v, depth + 1),
            Variant::ExpandedNodeId(v) => self.expanded_id(v, depth + 1),
            Variant::QualifiedName(v) => {
                let mut object = self.typed("QualifiedName", dense)?;
                self.field(&mut object, "namespace_index", &v.namespace_index)?;
                self.add(&mut object, "name", |budget| {
                    budget.nullable_text("String", &v.name, true)
                })?;
                Ok(Value::Object(object))
            }
            Variant::LocalizedText(v) => {
                let mut object = self.typed("LocalizedText", dense)?;
                self.add(&mut object, "locale", |budget| {
                    budget.nullable_text("String", &v.locale, true)
                })?;
                self.add(&mut object, "text", |budget| {
                    budget.nullable_text("String", &v.text, true)
                })?;
                Ok(Value::Object(object))
            }
            Variant::ExtensionObject(v) => self.extension(v, depth + 1),
            Variant::Variant(v) => {
                let mut object = self.typed("Variant", false)?;
                self.add(&mut object, "value", |budget| {
                    budget.variant(v, depth + 1, false)
                })?;
                Ok(Value::Object(object))
            }
            Variant::DataValue(v) => self.data(v, depth + 1),
            Variant::DiagnosticInfo(v) => self.diagnostic(v, depth + 1),
            Variant::Array(v) => self.array(v, depth + 1),
        }
    }
}

fn ticks(value: &opcua::types::DateTime) -> Result<i64> {
    // Checked wide arithmetic avoids overflow at the signed tick endpoints and
    // retains the SDK's decoded time instead of its clamping wire encoder.
    let time = value.as_chrono();
    let epoch = opcua::types::DateTime::epoch().as_chrono();
    let seconds = i128::from(time.timestamp()) - i128::from(epoch.timestamp());
    let ticks = seconds * 10_000_000 + i128::from(time.timestamp_subsec_nanos() / 100);
    i64::try_from(ticks).map_err(|_| invalid("OPC-UA decoded timestamp exceeds signed tick range"))
}

/// Full SDK-exposed data and quality; output never substitutes a missing value.
pub(super) fn data_value(value: &DataValue, deadline: Instant) -> Result<Value> {
    let mut budget = Budget::new(deadline);
    let result = budget.data(value, 0)?;
    check_time(deadline)?;
    Ok(result)
}
/// Preserve diagnostic indexes separately from their response string table.
pub(super) fn diagnostic_info(value: &DiagnosticInfo, deadline: Instant) -> Result<Value> {
    let mut budget = Budget::new(deadline);
    let result = budget.diagnostic(value, 0)?;
    check_time(deadline)?;
    Ok(result)
}

/// Preserve well-formed response-header extensions instead of dropping vendor data.
pub(super) fn extension_object(value: &ExtensionObject, deadline: Instant) -> Result<Value> {
    let mut budget = Budget::new(deadline);
    let result = budget.extension(value, 0)?;
    check_time(deadline)?;
    Ok(result)
}

#[cfg(test)]
mod tests;
