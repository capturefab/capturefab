//! The parts of the native `src/genicam.rs` that a feature write goes
//! through, for the Pattern camera's fixed node map: typed input parsing
//! (`input_value`, `integer_literal`, `literal`), bounds checks
//! (`validate_numeric`) with their exact messages, `stream_locked`, and the
//! `FeatureInfo` shape.
use crate::Result;
use crate::json::Json;

/// A JSON value as serde_json holds it, so numbers keep their integer or
/// float identity through validation and back out as JSON.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    PosInt(u64),
    NegInt(i64),
    Float(f64),
    Str(String),
}

impl Value {
    /// `json!(i64)`.
    pub fn int(value: i64) -> Self {
        if value < 0 {
            Self::NegInt(value)
        } else {
            Self::PosInt(value as u64)
        }
    }
    /// `json!(f64)`: non-finite floats become null.
    pub fn float(value: f64) -> Self {
        if value.is_finite() {
            Self::Float(value)
        } else {
            Self::Null
        }
    }
    pub fn as_i64(&self) -> Option<i64> {
        match *self {
            Self::PosInt(v) => i64::try_from(v).ok(),
            Self::NegInt(v) => Some(v),
            _ => None,
        }
    }
    pub fn as_u64(&self) -> Option<u64> {
        match *self {
            Self::PosInt(v) => Some(v),
            _ => None,
        }
    }
    pub fn as_f64(&self) -> Option<f64> {
        match *self {
            Self::PosInt(v) => Some(v as f64),
            Self::NegInt(v) => Some(v as f64),
            Self::Float(v) => Some(v),
            _ => None,
        }
    }
    pub fn as_bool(&self) -> Option<bool> {
        match *self {
            Self::Bool(v) => Some(v),
            _ => None,
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(v) => Some(v),
            _ => None,
        }
    }
    pub fn write(&self, json: &mut Json) {
        match self {
            Self::Null => json.null(),
            Self::Bool(v) => json.bool(*v),
            Self::PosInt(v) => json.u64(*v),
            Self::NegInt(v) => json.i64(*v),
            Self::Float(v) => json.f64(*v),
            Self::Str(v) => json.string(v),
        }
    }
}

/// Features a camera cannot change while it streams: the image's geometry
/// and format.
pub fn stream_locked(name: &str) -> bool {
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

pub fn integer_kind(kind: &str) -> bool {
    matches!(kind, "Integer" | "IntReg" | "MaskedIntReg" | "IntConverter")
}

pub fn number(v: &Value) -> Result<f64> {
    let n = v
        .as_f64()
        .or_else(|| v.as_bool().map(|b| if b { 1.0 } else { 0.0 }))
        .ok_or_else(|| "expected numeric value".to_string())?;
    ensure!(n.is_finite(), "expected finite number");
    Ok(n)
}

/// A user's text as the value a node of `kind` takes.
pub fn input_value(name: &str, kind: &str, text: &str) -> Result<Value> {
    match kind {
        "String" | "StringReg" | "Enumeration" => Ok(Value::Str(text.into())),
        "Boolean" => match text.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "on" => Ok(Value::Bool(true)),
            "false" | "0" | "off" => Ok(Value::Bool(false)),
            _ => bail!("{name} must be true or false (also accepts 1/0 and on/off)"),
        },
        "Command" => bail!("use execute for command {name}"),
        _ if integer_kind(kind) => integer_literal(text)
            .map_err(|error| format!("{name} requires an exact integer: {error}")),
        _ => literal(text).map_err(|error| format!("{name} requires a finite number: {error}")),
    }
}

/// Parse integral decimal, scientific and hexadecimal forms without first
/// rounding through f64.
pub fn integer_literal(text: &str) -> Result<Value> {
    let text = text.trim();
    let (negative, body) = if let Some(body) = text.strip_prefix('-') {
        (true, body)
    } else {
        (false, text.strip_prefix('+').unwrap_or(text))
    };
    let magnitude = if let Some(hex) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16)
            .map_err(|error| format!("invalid hexadecimal integer or integer overflow: {error}"))?
    } else {
        let mut parts = body.split(['e', 'E']);
        let mantissa = parts.next().unwrap_or("");
        let exponent = parts
            .next()
            .map(|s| s.parse::<i32>())
            .transpose()
            .map_err(|error| format!("invalid integer exponent: {error}"))?
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
        let mut digits = format!("{whole}{fraction}")
            .trim_start_matches('0')
            .to_owned();
        if digits.is_empty() {
            return Ok(Value::PosInt(0));
        }
        let shift = i64::from(exponent) - fraction.len() as i64;
        if shift < 0 {
            let remove = usize::try_from(-shift)
                .map_err(|error| format!("integer exponent overflow: {error}"))?;
            ensure!(
                remove <= digits.len()
                    && digits[digits.len() - remove..].bytes().all(|b| b == b'0'),
                "fractional values are not allowed"
            );
            digits.truncate(digits.len() - remove);
        } else {
            ensure!(
                shift <= 20 && digits.len() + shift as usize <= 20,
                "integer overflow"
            );
            digits.extend(std::iter::repeat_n('0', shift as usize));
        }
        digits
            .parse::<u64>()
            .map_err(|error| format!("integer overflow: {error}"))?
    };
    if negative {
        let value = -i128::from(magnitude);
        ensure!(value >= i128::from(i64::MIN), "signed integer overflow");
        Ok(Value::int(value as i64))
    } else {
        Ok(Value::PosInt(magnitude))
    }
}

/// A number in any of the forms GenICam XML and users write.
pub fn literal(s: &str) -> Result<Value> {
    let s = s.trim();
    let body = s.strip_prefix(['+', '-']).unwrap_or(s);
    if body.starts_with("0x") || body.starts_with("0X") {
        return integer_literal(s);
    }
    if let Ok(i) = s.parse::<i64>() {
        return Ok(Value::int(i));
    }
    if let Ok(u) = s.parse::<u64>() {
        return Ok(Value::PosInt(u));
    }
    if let Ok(integer) = integer_literal(s) {
        return Ok(integer);
    }
    let n: f64 = s
        .parse()
        .map_err(|error| format!("invalid numeric value: {error}"))?;
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
    Ok(Value::float(n))
}

fn exact_integer(value: &Value) -> Result<i128> {
    if let Some(value) = value.as_i64() {
        return Ok(i128::from(value));
    }
    if let Some(value) = value.as_u64() {
        return Ok(i128::from(value));
    }
    let value = value
        .as_f64()
        .ok_or_else(|| "expected an integer".to_string())?;
    ensure!(
        value.is_finite() && value.fract() == 0.0 && value.abs() <= (1u64 << 53) as f64,
        "expected an exactly representable integer"
    );
    Ok(value as i128)
}

/// Check `value` against a node's Min, Max and Inc.
pub fn validate_numeric(
    name: &str,
    kind: &str,
    value: &Value,
    min: Option<Value>,
    max: Option<Value>,
    inc: Option<Value>,
) -> Result<()> {
    if integer_kind(kind) {
        let value =
            exact_integer(value).map_err(|error| format!("{name} requires an integer: {error}"))?;
        let min = min.as_ref().map(exact_integer).transpose()?;
        let max = max.as_ref().map(exact_integer).transpose()?;
        ensure!(
            !min.zip(max).is_some_and(|(low, high)| low > high),
            "{name} has invalid camera bounds"
        );
        if let Some(min) = min {
            ensure!(
                value >= min,
                "{name} violates Min={min}; enter {min} or greater"
            );
        }
        if let Some(max) = max {
            ensure!(
                value <= max,
                "{name} violates Max={max}; enter {max} or less"
            );
        }
        if let Some(inc) = inc.as_ref().map(exact_integer).transpose()? {
            ensure!(inc > 0, "{name} has invalid camera increment {inc}");
            let base = min.unwrap_or(0);
            ensure!(
                (value - base) % inc == 0,
                "{name} requires increment {inc} from {base}"
            );
        }
    } else {
        let value = number(value)?;
        let min = min.as_ref().map(number).transpose()?;
        let max = max.as_ref().map(number).transpose()?;
        ensure!(
            !min.zip(max).is_some_and(|(low, high)| low > high),
            "{name} has invalid camera bounds"
        );
        if let Some(min) = min {
            ensure!(
                value >= min,
                "{name} violates Min={min}; enter {min} or greater"
            );
        }
        if let Some(max) = max {
            ensure!(
                value <= max,
                "{name} violates Max={max}; enter {max} or less"
            );
        }
        if let Some(inc) = inc.as_ref().map(number).transpose()? {
            ensure!(inc > 0.0, "{name} has invalid camera increment {inc}");
            let base = min.unwrap_or(0.0);
            let steps = (value - base) / inc;
            let nearest = base + steps.round() * inc;
            let tolerance = (8.0 * f64::EPSILON * value.abs().max(base.abs()).max(nearest.abs()))
                .min(inc * 1e-7);
            ensure!(
                nearest.is_finite() && (value - nearest).abs() <= tolerance,
                "{name} requires increment {inc} from {base}; nearest value is {nearest}"
            );
        }
    }
    Ok(())
}

/// A register write's integer as an unsigned value.
pub fn unsigned_integer(v: &Value) -> Result<u64> {
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

/// One feature as the desktop lists it (`genicam::FeatureInfo`), plus the
/// tools layer's `locked_while_streaming` (`stream_locked`, independent of
/// whether the camera streams now).
#[derive(Clone, Debug, PartialEq)]
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
    pub inc: Option<f64>,
    pub choices: Vec<String>,
    pub error: Option<String>,
}

impl FeatureInfo {
    /// The native JSON fields in declaration order, then
    /// `locked_while_streaming`.
    pub fn write(&self, json: &mut Json) {
        let option_f64 = |json: &mut Json, key: &str, value: Option<f64>| {
            json.key(key);
            match value {
                Some(v) => json.f64(v),
                None => json.null(),
            }
        };
        json.begin_object();
        json.field_str("name", &self.name);
        json.field_str("display_name", &self.display_name);
        json.field_str("kind", &self.kind);
        json.key("value");
        match &self.value {
            Some(value) => value.write(json),
            None => json.null(),
        }
        json.key("writable");
        json.bool(self.writable);
        json.field_str("description", &self.description);
        json.key("unit");
        match &self.unit {
            Some(unit) => json.string(unit),
            None => json.null(),
        }
        option_f64(json, "min", self.min);
        option_f64(json, "max", self.max);
        option_f64(json, "inc", self.inc);
        json.key("choices");
        json.begin_array();
        for choice in &self.choices {
            json.string(choice);
        }
        json.end_array();
        json.key("error");
        match &self.error {
            Some(error) => json.string(error),
            None => json.null(),
        }
        json.key("locked_while_streaming");
        json.bool(stream_locked(&self.name));
        json.end_object();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literals_parse_like_genicam() {
        assert_eq!(integer_literal("0x320"), Ok(Value::PosInt(800)));
        assert_eq!(integer_literal(" 1e3 "), Ok(Value::PosInt(1000)));
        assert_eq!(integer_literal("2.50e1"), Ok(Value::PosInt(25)));
        assert_eq!(integer_literal("-5"), Ok(Value::NegInt(-5)));
        assert_eq!(integer_literal("-0"), Ok(Value::PosInt(0)));
        assert_eq!(
            integer_literal("800.5").unwrap_err(),
            "fractional values are not allowed"
        );
        assert_eq!(
            integer_literal("abc").unwrap_err(),
            "expected a decimal or hexadecimal integer"
        );
        assert_eq!(literal("6"), Ok(Value::PosInt(6)));
        assert_eq!(literal("3.25"), Ok(Value::Float(3.25)));
        assert_eq!(literal("-1.5"), Ok(Value::Float(-1.5)));
        assert_eq!(literal("nan").unwrap_err(), "expected finite number");
        assert_eq!(
            literal("x").unwrap_err(),
            "invalid numeric value: invalid float literal"
        );
        assert_eq!(
            input_value("Width", "Integer", "abc").unwrap_err(),
            "Width requires an exact integer: expected a decimal or hexadecimal integer"
        );
        assert_eq!(
            input_value("Start", "Command", "1").unwrap_err(),
            "use execute for command Start"
        );
    }

    #[test]
    fn bounds_messages_match_the_desktop() {
        let int = |v: i64| Some(Value::int(v));
        assert_eq!(
            validate_numeric(
                "Width",
                "Integer",
                &Value::PosInt(9999),
                int(64),
                int(1920),
                int(1)
            )
            .unwrap_err(),
            "Width violates Max=1920; enter 1920 or less"
        );
        assert_eq!(
            validate_numeric(
                "Width",
                "Integer",
                &Value::PosInt(65),
                int(64),
                int(1920),
                int(2)
            )
            .unwrap_err(),
            "Width requires increment 2 from 64"
        );
        assert_eq!(
            validate_numeric(
                "Gain",
                "Float",
                &Value::Float(24.5),
                Some(Value::PosInt(0)),
                Some(Value::PosInt(24)),
                None
            )
            .unwrap_err(),
            "Gain violates Max=24; enter 24 or less"
        );
        assert_eq!(
            validate_numeric(
                "Gain",
                "Float",
                &Value::Float(0.3),
                None,
                None,
                Some(Value::Float(0.25))
            )
            .unwrap_err(),
            "Gain requires increment 0.25 from 0; nearest value is 0.25"
        );
    }
}
