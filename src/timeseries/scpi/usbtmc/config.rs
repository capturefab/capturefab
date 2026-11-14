//! Exact configured USBTMC identity; validation performs no device operations.
use crate::types::Usage;
use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Config {
    pub(crate) vendor_id: u16,
    pub(crate) product_id: u16,
    pub(crate) serial_number: String,
    pub(crate) interface_number: u8,
}

impl<'de> Deserialize<'de> for Config {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            #[serde(deserialize_with = "crate::config_number::unsigned")]
            vendor_id: u16,
            #[serde(deserialize_with = "crate::config_number::unsigned")]
            product_id: u16,
            serial_number: String,
            #[serde(deserialize_with = "crate::config_number::unsigned")]
            interface_number: u8,
        }
        let value = crate::session::strict_json_value(deserializer)?;
        if !value.is_object() {
            return Err(serde::de::Error::custom(
                "USBTMC selector must be an object",
            ));
        }
        let raw: Raw = serde_json::from_value(value).map_err(serde::de::Error::custom)?;
        let value = Self {
            vendor_id: raw.vendor_id,
            product_id: raw.product_id,
            serial_number: raw.serial_number,
            interface_number: raw.interface_number,
        };
        value.validate().map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}

impl Config {
    pub(crate) fn validate(&self) -> Result<()> {
        if self.vendor_id == 0 {
            return Err(Usage("USBTMC vendor_id must be a nonzero u16".into()).into());
        }
        // USBTMC 1.0 section 5.7/Table 45: exact US-English serial, without
        // trimming/case folding or an inferred bus/index replacement.
        if self.serial_number.is_empty()
            || self.serial_number.len() > 63
            || self.serial_number.starts_with(' ')
            || self.serial_number.ends_with(' ')
            || !self.serial_number.bytes().all(|byte| {
                (0x20..=0x7e).contains(&byte)
                    && !matches!(byte, b'"' | b'*' | b'/' | b':' | b'?' | b'\\')
            })
        {
            return Err(Usage(
                "USBTMC serial_number requires 1–63 printable ASCII characters, no edge spaces and no double quote, star, slash, colon, question mark or backslash".into(),
            )
            .into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn selector() -> Value {
        json!({"vendor_id":4660,"product_id":22136,"serial_number":"TMC-Example 1<>|","interface_number":3})
    }

    #[test]
    fn selector_keeps_exact_identity_and_unsigned_numeric_bounds() {
        let value = selector();
        let config: Config = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(&config).unwrap(), value);
        assert_eq!(config.serial_number, "TMC-Example 1<>|");
        let config: Config = serde_json::from_str(
            r#"{"vendor_id":4.660e3,"product_id":0.0,"serial_number":"a","interface_number":2.55e2}"#,
        )
        .unwrap();
        assert_eq!(config.vendor_id, 4660);
        assert_eq!(config.product_id, 0);
        assert_eq!(config.interface_number, 255);
        config.validate().unwrap();
        let mut value = selector();
        value["vendor_id"] = json!(65_535);
        value["product_id"] = json!(65_535);
        value["serial_number"] = json!("x".repeat(63));
        serde_json::from_value::<Config>(value).unwrap();
    }

    #[test]
    fn serial_uses_the_six_specified_punctuation_exclusions_without_normalizing() {
        for serial in ["", " leading", "trailing ", "\t", "\n", "\u{7f}", "µ"] {
            let mut value = selector();
            value["serial_number"] = json!(serial);
            serde_json::from_value::<Config>(value).unwrap_err();
        }
        for forbidden in ['"', '*', '/', ':', '?', '\\'] {
            let mut value = selector();
            value["serial_number"] = json!(format!("before{forbidden}after"));
            serde_json::from_value::<Config>(value).unwrap_err();
        }
        let mut value = selector();
        value["serial_number"] = json!("x".repeat(64));
        serde_json::from_value::<Config>(value).unwrap_err();
        for serial in [
            "a",
            "A",
            "<>",
            "|",
            "a  b",
            "!#$%&'()+,-.0123456789;=@[]^_`{}~",
        ] {
            let mut value = selector();
            value["serial_number"] = json!(serial);
            let config: Config = serde_json::from_value(value).unwrap();
            assert_eq!(config.serial_number, serial);
        }
    }

    #[test]
    fn selector_rejects_missing_null_duplicate_unknown_and_inexact_fields() {
        let value = selector();
        for name in [
            "vendor_id",
            "product_id",
            "serial_number",
            "interface_number",
        ] {
            let mut missing = value.clone();
            missing.as_object_mut().unwrap().remove(name);
            serde_json::from_value::<Config>(missing).unwrap_err();
            let mut null = value.clone();
            null[name] = Value::Null;
            serde_json::from_value::<Config>(null).unwrap_err();
        }
        for name in [
            "bus",
            "index",
            "path",
            "transport",
            "detach",
            "alternate_setting",
        ] {
            let mut extra = value.clone();
            extra[name] = json!(0);
            serde_json::from_value::<Config>(extra).unwrap_err();
        }
        for (name, invalid) in [
            ("vendor_id", json!(0)),
            ("vendor_id", json!(65_536)),
            ("product_id", json!(65_536)),
            ("interface_number", json!(256)),
            ("vendor_id", json!(-1)),
            ("product_id", json!(0.5)),
            ("interface_number", json!("3")),
            ("interface_number", json!(true)),
        ] {
            let mut bad = value.clone();
            bad[name] = invalid;
            serde_json::from_value::<Config>(bad).unwrap_err();
        }
        serde_json::from_str::<Config>(
            r#"{"vendor_id":1,"vendor_id":2,"product_id":0,"serial_number":"a","interface_number":0}"#,
        ).unwrap_err();
        for value in [
            json!([4660, 22136, "SN", 3]),
            json!([]),
            json!("selector"),
            Value::Null,
        ] {
            serde_json::from_value::<Config>(value).unwrap_err();
        }
    }
}
