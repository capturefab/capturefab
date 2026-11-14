//! Explicit PF6000 compatibility profile; validation performs no I/O.
use crate::types::Usage;
use anyhow::Result;
use serde::Deserialize;
use std::net::{IpAddr, SocketAddr};

#[derive(Debug, Clone, Copy, Deserialize)]
enum Profile {
    #[serde(rename = "pf6000_v2_8")]
    Pf6000V28,
}

#[derive(Debug, Clone)]
pub(crate) struct Config {
    pub(crate) address: SocketAddr,
    pub(crate) signal: String,
    profile: Profile,
}

fn default_signal() -> String {
    "tightening".into()
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
            #[serde(default = "default_signal")]
            signal: String,
        }
        // Serde structs can also deserialize positional arrays. This public
        // selector is a closed object, including when used outside Adapter.
        let value = crate::session::strict_json_value(deserializer)?;
        if !value.is_object() {
            return Err(serde::de::Error::custom(
                "Atlas Open Protocol configuration must be an object",
            ));
        }
        let raw: Raw = serde_json::from_value(value).map_err(serde::de::Error::custom)?;
        let config = Self {
            address: raw.address,
            signal: raw.signal,
            profile: raw.profile,
        };
        config.validate().map_err(serde::de::Error::custom)?;
        Ok(config)
    }
}

impl Config {
    pub(crate) fn validate(&self) -> Result<()> {
        // A mapped address still targets its IPv4 peer. Preserve the requested
        // SocketAddr, but apply the same admission rules to both spellings.
        let ip = self.address.ip().to_canonical();
        if self.address.port() == 0
            || ip.is_unspecified()
            || ip.is_multicast()
            || matches!(ip, IpAddr::V4(address) if address.is_broadcast())
        {
            return Err(Usage(
                "Atlas Open Protocol requires an explicit numeric unicast IP:port".into(),
            )
            .into());
        }
        match self.profile {
            Profile::Pf6000V28 => super::super::validate_name(&self.signal, "Atlas signal"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn selector() -> Value {
        json!({"profile":"pf6000_v2_8","address":"127.0.0.1:4545"})
    }

    #[test]
    fn explicit_profile_defaults_only_the_signal_and_preserves_requested_identity() {
        let config: Config = serde_json::from_value(selector()).unwrap();
        config.validate().unwrap();
        assert_eq!(config.signal, "tightening");
        assert_eq!(config.address.to_string(), "127.0.0.1:4545");
        let mut value = selector();
        value["address"] = json!("[::1]:65535");
        value["signal"] = json!("  exact signal  ");
        let config: Config = serde_json::from_value(value).unwrap();
        assert_eq!(config.signal, "  exact signal  ");
        assert_eq!(config.address.to_string(), "[::1]:65535");
        let mut value = selector();
        value["address"] = json!("[::ffff:127.0.0.1]:4545");
        let config: Config = serde_json::from_value(value).unwrap();
        assert_eq!(config.address.to_string(), "[::ffff:127.0.0.1]:4545");
    }

    #[test]
    fn selector_is_a_closed_object_with_required_profile_and_no_null_defaults() {
        for value in [
            Value::Null,
            json!(["pf6000_v2_8", "127.0.0.1:4545", "tightening"]),
            json!({"address":"127.0.0.1:4545"}),
            json!({"profile":"pf6000_v2_8"}),
        ] {
            serde_json::from_value::<Config>(value).unwrap_err();
        }
        for key in ["profile", "address", "signal"] {
            let mut value = selector();
            value[key] = Value::Null;
            serde_json::from_value::<Config>(value).unwrap_err();
        }
        for key in ["fields", "mid", "credentials", "port", "reconnect"] {
            let mut value = selector();
            value[key] = json!(false);
            serde_json::from_value::<Config>(value).unwrap_err();
        }
        serde_json::from_str::<Config>(
            r#"{"profile":"pf6000_v2_8","address":"127.0.0.1:4545","signal":"a","signal":"b"}"#,
        )
        .unwrap_err();
    }

    #[test]
    fn selector_rejects_implicit_or_nonunicast_endpoints_and_unselected_profiles() {
        for address in [
            "localhost:4545",
            "127.0.0.1",
            "127.0.0.1:0",
            "0.0.0.0:4545",
            "239.1.2.3:4545",
            "255.255.255.255:4545",
            "[::]:4545",
            "[ff02::1]:4545",
            "[::ffff:0.0.0.0]:4545",
            "[::ffff:239.1.2.3]:4545",
            "[::ffff:255.255.255.255]:4545",
            "tcp://127.0.0.1:4545",
        ] {
            let mut value = selector();
            value["address"] = json!(address);
            serde_json::from_value::<Config>(value).unwrap_err();
        }
        for profile in ["pf6000", "pf6000_v3_16", "auto", ""] {
            let mut value = selector();
            value["profile"] = json!(profile);
            serde_json::from_value::<Config>(value).unwrap_err();
        }
    }

    #[test]
    fn signal_uses_existing_bounded_noncontrol_name_policy() {
        for signal in [String::new(), "\n".into(), "x".repeat(1025)] {
            let mut value = selector();
            value["signal"] = json!(signal);
            serde_json::from_value::<Config>(value).unwrap_err();
        }
        let mut value = selector();
        value["signal"] = json!("x".repeat(1024));
        serde_json::from_value::<Config>(value).unwrap();
    }
}
