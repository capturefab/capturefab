//! Exact integral decimal/exponent input for declared configuration fields.
//! Runtime JSON values are never traversed, rewritten or converted through f64.
use serde::Deserializer;
use serde_json::Value;
use std::fmt::Display;

fn exact_unsigned(value: &Value) -> Result<u64, &'static str> {
    let invalid = "expected an exact unsigned integer in range";
    let number = value.as_number().ok_or(invalid)?.to_string();
    let (mantissa, exponent) = number
        .split_once(['e', 'E'])
        .unwrap_or((number.as_str(), "0"));
    let negative = mantissa.starts_with('-');
    let mantissa = mantissa.strip_prefix('-').unwrap_or(mantissa);
    let fraction = mantissa
        .split_once('.')
        .map_or(0, |(_, fraction)| fraction.len());
    let digits = mantissa.replace('.', "");
    let mut digits = digits.trim_start_matches('0');
    if digits.is_empty() {
        return Ok(0);
    }
    if negative {
        return Err(invalid);
    }
    let scale = exponent
        .parse::<i64>()
        .ok()
        .and_then(|exponent| exponent.checked_sub(i64::try_from(fraction).ok()?))
        .ok_or(invalid)?;
    let zeros = if scale < 0 {
        let end = digits
            .len()
            .checked_sub(usize::try_from(scale.unsigned_abs()).map_err(|_| invalid)?)
            .ok_or(invalid)?;
        if !digits
            .get(end..)
            .ok_or(invalid)?
            .bytes()
            .all(|byte| byte == b'0')
        {
            return Err(invalid);
        }
        digits = digits.get(..end).ok_or(invalid)?;
        0
    } else {
        usize::try_from(scale).map_err(|_| invalid)?
    };
    if digits.len().saturating_add(zeros) > 20 {
        return Err(invalid);
    }
    let mut value = 0_u64;
    for digit in digits.bytes().chain(std::iter::repeat_n(b'0', zeros)) {
        value = value
            .checked_mul(10)
            .and_then(|value| value.checked_add(u64::from(digit - b'0')))
            .ok_or(invalid)?;
    }
    Ok(value)
}

pub(crate) fn unsigned<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: TryFrom<u64>,
    T::Error: Display,
{
    let input = crate::session::strict_json_value(deserializer)?;
    let integer = exact_unsigned(&input).map_err(serde::de::Error::custom)?;
    T::try_from(integer).map_err(|error| {
        serde::de::Error::custom(format!(
            "expected an exact unsigned integer in range: {error}"
        ))
    })
}

pub(crate) fn optional_unsigned<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: TryFrom<u64>,
    T::Error: Display,
{
    let input = crate::session::strict_json_value(deserializer)?;
    if input.is_null() {
        return Ok(None);
    }
    let integer = exact_unsigned(&input).map_err(serde::de::Error::custom)?;
    T::try_from(integer).map(Some).map_err(|error| {
        serde::de::Error::custom(format!(
            "expected an exact unsigned integer in range: {error}"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::{optional_unsigned, unsigned};
    use serde::Deserialize;
    use serde_json::Value;

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Example {
        #[serde(default = "default_count", deserialize_with = "unsigned")]
        count: u64,
        #[serde(default, deserialize_with = "optional_unsigned")]
        byte: Option<u8>,
        payload: Value,
    }

    const fn default_count() -> u64 {
        100
    }

    #[test]
    fn typed_counts_remain_exact_without_changing_payloads_or_field_validation() {
        let input = r#"{"count":1.8446744073709551615e19,"byte":2.55e2,"payload":{"number":0.12345678901234567890123456789}}"#;
        let config: Example = serde_json::from_str(input).unwrap();
        assert_eq!(config.count, u64::MAX);
        assert_eq!(config.byte, Some(u8::MAX));
        assert_eq!(
            config.payload["number"].to_string(),
            "0.12345678901234567890123456789"
        );
        for count in [
            "1.00000000000000000001",
            "18446744073709551616",
            "1e1000",
            "1e-1000",
            "-1",
            "true",
            "null",
            "\"1\"",
            "{}",
        ] {
            let input = format!(r#"{{"count":{count},"payload":null}}"#);
            serde_json::from_str::<Example>(&input).unwrap_err();
        }
        for input in [
            r#"{"count":1,"count":2,"payload":null}"#,
            r#"{"count":1,"payload":null,"unknown":2}"#,
            r#"{"count":1,"byte":256.0,"payload":null}"#,
        ] {
            serde_json::from_str::<Example>(input).unwrap_err();
        }
        let config: Example =
            serde_json::from_str(r#"{"count":9.007199254740993e15,"byte":null,"payload":0}"#)
                .unwrap();
        assert_eq!(config.count, 9_007_199_254_740_993);
        assert_eq!(config.byte, None);
        for (token, expected) in [
            ("-0", 0),
            ("-0.000e-999999999999999999999", 0),
            ("18446744073709551615.0", u64::MAX),
            ("184467440737095516150e-1", u64::MAX),
        ] {
            let input = format!(r#"{{"count":{token},"payload":null}}"#);
            let direct: Example = serde_json::from_str(&input).unwrap();
            let through_value: Example =
                serde_json::from_value(serde_json::from_str::<Value>(&input).unwrap()).unwrap();
            assert_eq!(direct.count, expected, "{token}");
            assert_eq!(through_value.count, expected, "{token}");
        }
        let omitted: Example = serde_json::from_str(r#"{"payload":null}"#).unwrap();
        assert_eq!(omitted.count, 100);
        assert_eq!(omitted.byte, None);
    }
}
