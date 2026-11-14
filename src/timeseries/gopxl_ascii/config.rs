//! Explicit `GoPxL` Ethernet ASCII measurement selectors; validation performs no I/O.
use crate::types::Usage;
use anyhow::Result;
use serde::Deserialize;
use std::{
    collections::BTreeSet,
    net::{IpAddr, SocketAddr},
};

const MAX_MEASUREMENTS: usize = 64;
/// A software bound; the manufacturer documents no maximum measurement ID.
const MAX_ID: u32 = 65_535;
const MAX_TOKEN: usize = 32;

#[derive(Debug, Clone, Copy, Deserialize)]
enum Profile {
    #[serde(rename = "gopxl_1_3_measurement")]
    Gopxl13Measurement,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Terminator {
    Crlf,
    Cr,
    Lf,
}
impl Terminator {
    pub(crate) const fn bytes(self) -> &'static [u8] {
        match self {
            Self::Crlf => b"\r\n",
            Self::Cr => b"\r",
            Self::Lf => b"\n",
        }
    }
}

/// The manufacturer's examples contradict each other on a trailing delimiter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Trailing {
    Absent,
    Present,
    Either,
}

#[derive(Debug, Clone)]
pub(crate) struct Measurement {
    pub(crate) name: String,
    pub(crate) id: u32,
}

#[derive(Debug, Clone)]
pub(crate) struct Config {
    pub(crate) address: SocketAddr,
    pub(crate) delimiter: u8,
    pub(crate) terminator: Terminator,
    pub(crate) invalid_token: String,
    pub(crate) trailing_delimiter: Trailing,
    pub(crate) measurements: Vec<Measurement>,
    profile: Profile,
}

impl<'de> Deserialize<'de> for Measurement {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            name: String,
            #[serde(deserialize_with = "crate::config_number::unsigned")]
            id: u32,
        }
        let value = crate::session::strict_json_value(deserializer)?;
        if !value.is_object() {
            return Err(serde::de::Error::custom(
                "GoPxL measurement must be an object",
            ));
        }
        let raw: Raw = serde_json::from_value(value).map_err(serde::de::Error::custom)?;
        Ok(Self {
            name: raw.name,
            id: raw.id,
        })
    }
}

impl<'de> Deserialize<'de> for Config {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            profile: Profile,
            address: SocketAddr,
            delimiter: String,
            terminator: Terminator,
            invalid_token: String,
            trailing_delimiter: Trailing,
            measurements: Vec<Measurement>,
        }
        let value = crate::session::strict_json_value(deserializer)?;
        if !value.is_object() {
            return Err(serde::de::Error::custom(
                "GoPxL ASCII configuration must be an object",
            ));
        }
        let raw: Raw = serde_json::from_value(value).map_err(serde::de::Error::custom)?;
        let [delimiter] = *raw.delimiter.as_bytes() else {
            return Err(serde::de::Error::custom(
                "GoPxL delimiter must be exactly one ASCII character",
            ));
        };
        let config = Self {
            address: raw.address,
            delimiter,
            terminator: raw.terminator,
            invalid_token: raw.invalid_token,
            trailing_delimiter: raw.trailing_delimiter,
            measurements: raw.measurements,
            profile: raw.profile,
        };
        config.validate().map_err(serde::de::Error::custom)?;
        Ok(config)
    }
}

/// `[+-]?[0-9]+`, which a value field could not distinguish from a number.
fn integer_like(token: &[u8]) -> bool {
    let digits = token
        .strip_prefix(b"-")
        .or_else(|| token.strip_prefix(b"+"))
        .unwrap_or(token);
    !digits.is_empty() && digits.iter().all(u8::is_ascii_digit)
}

impl Config {
    pub(crate) fn validate(&self) -> Result<()> {
        let Profile::Gopxl13Measurement = self.profile;
        let ip = self.address.ip().to_canonical();
        if self.address.port() == 0
            || ip.is_unspecified()
            || ip.is_multicast()
            || matches!(ip, IpAddr::V4(address) if address.is_broadcast())
        {
            return Err(Usage("GoPxL requires an explicit numeric unicast IP:port".into()).into());
        }
        // The delimiter must not collide with a field prefix, sign, digit or escape.
        let delimiter = self.delimiter;
        if !(0x21..=0x7e).contains(&delimiter)
            || delimiter.is_ascii_digit()
            || matches!(
                delimiter.to_ascii_uppercase(),
                b'%' | b'-' | b'+' | b'M' | b'V' | b'D' | b'O' | b'K' | b'E' | b'R'
            )
        {
            return Err(Usage(
                "GoPxL delimiter must be one printable character other than a digit, %, -, +, or a letter of M, V, D, OK or ERROR"
                    .into(),
            )
            .into());
        }
        let token = self.invalid_token.as_bytes();
        if token.is_empty()
            || token.len() > MAX_TOKEN
            || token.iter().any(|byte| !(0x21..=0x7e).contains(byte))
            || token.contains(&delimiter)
            || integer_like(token)
        {
            return Err(Usage(
                "GoPxL invalid_token must be 1–32 printable characters, not a number or the delimiter"
                    .into(),
            )
            .into());
        }
        if self.measurements.is_empty() || self.measurements.len() > MAX_MEASUREMENTS {
            return Err(Usage("GoPxL requires 1–64 measurements".into()).into());
        }
        let mut names = BTreeSet::new();
        let mut ids = BTreeSet::new();
        for measurement in &self.measurements {
            super::super::validate_name(&measurement.name, "GoPxL measurement name")?;
            if !names.insert(measurement.name.as_str()) {
                return Err(Usage(format!(
                    "duplicate GoPxL measurement name: {}",
                    measurement.name
                ))
                .into());
            }
            if measurement.id > MAX_ID || !ids.insert(measurement.id) {
                return Err(Usage(format!(
                    "GoPxL measurement IDs must be unique and at most {MAX_ID}: {}",
                    measurement.id
                ))
                .into());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn selector() -> Value {
        json!({
            "profile":"gopxl_1_3_measurement","address":"127.0.0.1:8190","delimiter":",",
            "terminator":"crlf","invalid_token":"INVALID","trailing_delimiter":"either",
            "measurements":[{"name":"height","id":0},{"name":"width","id":1}]
        })
    }
    fn rejected(value: &Value) {
        let result = serde_json::from_value::<Config>(value.clone());
        assert!(result.is_err(), "accepted {value}");
    }

    #[test]
    fn explicit_selector_is_accepted_and_preserved() {
        let config: Config = serde_json::from_value(selector()).unwrap();
        assert_eq!(config.delimiter, b',');
        assert_eq!(config.terminator.bytes(), b"\r\n");
        assert_eq!(config.trailing_delimiter, Trailing::Either);
        assert_eq!(
            config
                .measurements
                .iter()
                .map(|m| (m.name.as_str(), m.id))
                .collect::<Vec<_>>(),
            [("height", 0), ("width", 1)]
        );
    }

    #[test]
    fn every_framing_choice_is_required_and_closed() {
        for key in [
            "profile",
            "address",
            "delimiter",
            "terminator",
            "invalid_token",
            "trailing_delimiter",
            "measurements",
        ] {
            let mut value = selector();
            value[key] = Value::Null;
            rejected(&value);
            if let Some(map) = value.as_object_mut() {
                map.remove(key);
            }
            rejected(&value);
        }
        for (key, bad) in [
            ("profile", json!("gopxl")),
            ("terminator", json!("none")),
            ("terminator", json!("CRLF")),
            ("trailing_delimiter", json!("optional")),
            ("delimiter", json!(",,")),
            ("delimiter", json!("")),
            ("delimiter", json!(" ")),
            ("delimiter", json!("\t")),
            ("delimiter", json!("%")),
            ("delimiter", json!("-")),
            ("delimiter", json!("M")),
            ("delimiter", json!("d")),
            ("delimiter", json!("K")),
            ("delimiter", json!("r")),
            ("invalid_token", json!("0".repeat(32))),
            ("invalid_token", json!("9".repeat(32))),
            ("delimiter", json!("5")),
            ("delimiter", json!("é")),
            ("invalid_token", json!("")),
            ("invalid_token", json!("12")),
            ("invalid_token", json!("-7")),
            ("invalid_token", json!("IN,VALID")),
            ("invalid_token", json!("IN VALID")),
            ("invalid_token", json!("X".repeat(33))),
            ("address", json!("sensor.local:8190")),
            ("address", json!("127.0.0.1:0")),
            ("address", json!("255.255.255.255:8190")),
            ("command", json!("start")),
        ] {
            let mut value = selector();
            value[key] = bad;
            rejected(&value);
        }
        rejected(&json!(["gopxl_1_3_measurement"]));
    }

    #[test]
    fn measurements_are_bounded_unique_named_objects() {
        for measurements in [
            json!([]),
            json!([{"name":"a","id":0},{"name":"a","id":1}]),
            json!([{"name":"a","id":0},{"name":"b","id":0}]),
            json!([{"name":"a","id":65_536}]),
            json!([{"name":"a","id":-1}]),
            json!([{"name":"","id":1}]),
            json!([{"name":"a","id":1,"scale":1000}]),
            json!([["a", 1]]),
        ] {
            let mut value = selector();
            value["measurements"] = measurements;
            rejected(&value);
        }
        let mut value = selector();
        value["measurements"] = json!(
            (0..64)
                .map(|id| json!({"name":format!("m{id}"),"id":id}))
                .collect::<Vec<_>>()
        );
        serde_json::from_value::<Config>(value.clone()).unwrap();
        value["measurements"] = json!(
            (0..65)
                .map(|id| json!({"name":format!("m{id}"),"id":id}))
                .collect::<Vec<_>>()
        );
        rejected(&value);
    }
}
