//! Explicit groov expanded-input selectors; validation performs no I/O.
use super::model::{self, Model, Resource};
use crate::types::Usage;
use anyhow::Result;
use serde::Deserialize;
use std::{
    collections::BTreeSet,
    net::{IpAddr, SocketAddr},
};

const MAX_CHANNELS: usize = 64;

#[derive(Debug, Clone, Copy, Deserialize)]
enum Profile {
    #[serde(rename = "groov_expanded_inputs")]
    GroovExpandedInputs,
}

#[derive(Debug, Clone)]
pub(crate) struct Config {
    pub(crate) address: SocketAddr,
    pub(crate) model: Model,
    pub(crate) channels: Vec<Channel>,
    profile: Profile,
}

#[derive(Debug, Clone)]
pub(crate) struct Channel {
    pub(crate) name: String,
    pub(crate) module: u32,
    pub(crate) channel: u32,
    pub(crate) resource: Resource,
    pub(crate) expected_module_id: u32,
    pub(crate) expected_channel_type: u32,
}

#[derive(Deserialize)]
enum RawModel {
    #[serde(rename = "epic_pr1")]
    EpicPr1,
    #[serde(rename = "epic_pr2")]
    EpicPr2,
    #[serde(rename = "rio_mm1")]
    RioMm1,
    #[serde(rename = "rio_mm2")]
    RioMm2,
}
#[derive(Deserialize)]
enum RawResource {
    #[serde(rename = "analog_eu")]
    AnalogEu,
    #[serde(rename = "digital_state")]
    DigitalState,
}

fn object(value: &serde_json::Value, what: &str) -> std::result::Result<(), String> {
    if value.is_object() {
        Ok(())
    } else {
        Err(format!("OptoMMP {what} must be an object"))
    }
}

impl<'de> Deserialize<'de> for Channel {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            name: String,
            #[serde(deserialize_with = "crate::config_number::unsigned")]
            module: u32,
            #[serde(deserialize_with = "crate::config_number::unsigned")]
            channel: u32,
            resource: RawResource,
            #[serde(deserialize_with = "crate::config_number::unsigned")]
            expected_module_id: u32,
            #[serde(deserialize_with = "crate::config_number::unsigned")]
            expected_channel_type: u32,
        }
        let value = crate::session::strict_json_value(deserializer)?;
        object(&value, "channel").map_err(serde::de::Error::custom)?;
        let raw: Raw = serde_json::from_value(value).map_err(serde::de::Error::custom)?;
        Ok(Self {
            name: raw.name,
            module: raw.module,
            channel: raw.channel,
            resource: match raw.resource {
                RawResource::AnalogEu => Resource::AnalogEu,
                RawResource::DigitalState => Resource::DigitalState,
            },
            expected_module_id: raw.expected_module_id,
            expected_channel_type: raw.expected_channel_type,
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
            model: RawModel,
            address: SocketAddr,
            channels: Vec<Channel>,
        }
        // Serde structs also accept positional arrays; this selector is a closed object.
        let value = crate::session::strict_json_value(deserializer)?;
        object(&value, "configuration").map_err(serde::de::Error::custom)?;
        let raw: Raw = serde_json::from_value(value).map_err(serde::de::Error::custom)?;
        let config = Self {
            address: raw.address,
            model: match raw.model {
                RawModel::EpicPr1 => Model::EpicPr1,
                RawModel::EpicPr2 => Model::EpicPr2,
                RawModel::RioMm1 => Model::RioMm1,
                RawModel::RioMm2 => Model::RioMm2,
            },
            channels: raw.channels,
            profile: raw.profile,
        };
        config.validate().map_err(serde::de::Error::custom)?;
        Ok(config)
    }
}

impl Config {
    pub(crate) fn validate(&self) -> Result<()> {
        let Profile::GroovExpandedInputs = self.profile;
        // A mapped address still targets its IPv4 peer: keep the requested spelling
        // but apply the same admission rules to both.
        let ip = self.address.ip().to_canonical();
        if self.address.port() == 0
            || ip.is_unspecified()
            || ip.is_multicast()
            || matches!(ip, IpAddr::V4(address) if address.is_broadcast())
        {
            return Err(
                Usage("OptoMMP requires an explicit numeric unicast IP:port".into()).into(),
            );
        }
        if self.channels.is_empty() || self.channels.len() > MAX_CHANNELS {
            return Err(Usage("OptoMMP requires 1–64 channels".into()).into());
        }
        let mut names = BTreeSet::new();
        let mut physical = BTreeSet::new();
        for channel in &self.channels {
            super::super::validate_name(&channel.name, "OptoMMP channel name")?;
            if !names.insert(channel.name.as_str()) {
                return Err(
                    Usage(format!("duplicate OptoMMP channel name: {}", channel.name)).into(),
                );
            }
            if !physical.insert((channel.module, channel.channel)) {
                return Err(Usage(format!(
                    "duplicate OptoMMP physical channel: module {} channel {}",
                    channel.module, channel.channel
                ))
                .into());
            }
            if model::addresses(
                self.model,
                channel.module,
                channel.channel,
                channel.resource,
            )
            .is_none()
                || !model::allows_tuple(
                    self.model,
                    channel.module,
                    channel.channel,
                    channel.resource,
                    channel.expected_module_id,
                    channel.expected_channel_type,
                )
            {
                return Err(Usage(format!(
                    "OptoMMP channel {} (model {}, module {}, channel {}, {}, module ID {:#010x}, channel type {:#010x}) is not a documented groov expanded input",
                    channel.name,
                    self.model.name(),
                    channel.module,
                    channel.channel,
                    channel.resource.name(),
                    channel.expected_module_id,
                    channel.expected_channel_type
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
            "profile":"groov_expanded_inputs","model":"rio_mm1","address":"127.0.0.1:2001",
            "channels":[{"name":"ai0","module":0,"channel":0,"resource":"analog_eu",
                "expected_module_id":0xF000_0022_u32,"expected_channel_type":0x6000_0015_u32}]
        })
    }
    fn rejected(value: &Value) {
        let result = serde_json::from_value::<Config>(value.clone());
        assert!(result.is_err(), "accepted {value}");
    }

    #[test]
    fn explicit_selector_preserves_requested_identity() {
        let config: Config = serde_json::from_value(selector()).unwrap();
        assert_eq!(config.model, Model::RioMm1);
        assert_eq!(config.address.to_string(), "127.0.0.1:2001");
        let [channel] = config.channels.as_slice() else {
            panic!("one channel expected")
        };
        assert_eq!(
            (channel.module, channel.channel, channel.resource),
            (0, 0, Resource::AnalogEu)
        );
        assert_eq!(channel.expected_module_id, 0xF000_0022);
        assert_eq!(channel.expected_channel_type, 0x6000_0015);
        let mut value = selector();
        value["address"] = json!("[::ffff:127.0.0.1]:2001");
        let config: Config = serde_json::from_value(value).unwrap();
        assert_eq!(config.address.to_string(), "[::ffff:127.0.0.1]:2001");
    }

    #[test]
    fn selector_and_channels_are_closed_objects_without_defaults() {
        rejected(&Value::Null);
        rejected(&json!([
            "groov_expanded_inputs",
            "rio_mm1",
            "127.0.0.1:2001",
            []
        ]));
        for key in ["profile", "model", "address", "channels"] {
            let mut value = selector();
            value[key] = Value::Null;
            rejected(&value);
            if let Some(map) = value.as_object_mut() {
                map.remove(key);
            }
            rejected(&value);
        }
        for key in ["scale", "units", "raw_address", "retry", "reconnect"] {
            let mut value = selector();
            value[key] = json!(false);
            rejected(&value);
        }
        for key in [
            "name",
            "module",
            "channel",
            "resource",
            "expected_module_id",
            "expected_channel_type",
        ] {
            let mut value = selector();
            value["channels"][0][key] = Value::Null;
            rejected(&value);
            if let Some(map) = value["channels"][0].as_object_mut() {
                map.remove(key);
            }
            rejected(&value);
        }
        let mut value = selector();
        value["channels"][0]["address"] = json!("fffff0260000");
        rejected(&value);
        let mut value = selector();
        value["channels"] = json!([["ai0", 0, 0, "analog_eu", 0xF000_0022_u32, 0x6000_0015_u32]]);
        rejected(&value);
        serde_json::from_str::<Config>(
            r#"{"profile":"groov_expanded_inputs","model":"rio_mm1","model":"rio_mm2","address":"127.0.0.1:2001","channels":[{"name":"a","module":0,"channel":0,"resource":"analog_eu","expected_module_id":4026531874,"expected_channel_type":1610612757}]}"#,
        )
        .unwrap_err();
        serde_json::from_str::<Config>(
            r#"{"profile":"groov_expanded_inputs","model":"rio_mm1","address":"127.0.0.1:2001","channels":[{"name":"a","module":0,"channel":0,"channel":1,"resource":"analog_eu","expected_module_id":4026531874,"expected_channel_type":1610612757}]}"#,
        )
        .unwrap_err();
    }

    #[test]
    fn numbers_use_exact_unsigned_u32_parsing() {
        for number in [
            json!(-1),
            json!(0.5),
            json!("0"),
            json!(4_294_967_296_u64),
            json!(true),
        ] {
            let mut value = selector();
            value["channels"][0]["module"] = number.clone();
            rejected(&value);
            let mut value = selector();
            value["channels"][0]["expected_channel_type"] = number;
            rejected(&value);
        }
        let config: Config = serde_json::from_str(
            r#"{"profile":"groov_expanded_inputs","model":"rio_mm1","address":"127.0.0.1:2001","channels":[{"name":"a","module":0e0,"channel":0,"resource":"analog_eu","expected_module_id":4.026531874e9,"expected_channel_type":1610612757}]}"#,
        )
        .unwrap();
        assert_eq!(
            config.channels.first().map(|c| c.expected_module_id),
            Some(0xF000_0022)
        );
    }

    #[test]
    fn endpoints_profiles_models_and_resources_are_explicit() {
        for address in [
            "localhost:2001",
            "127.0.0.1",
            "127.0.0.1:0",
            "0.0.0.0:2001",
            "239.1.2.3:2001",
            "255.255.255.255:2001",
            "[::]:2001",
            "[ff02::1]:2001",
            "[::ffff:255.255.255.255]:2001",
        ] {
            let mut value = selector();
            value["address"] = json!(address);
            rejected(&value);
        }
        for profile in ["groov", "legacy", "auto", ""] {
            let mut value = selector();
            value["profile"] = json!(profile);
            rejected(&value);
        }
        for model in ["epic", "rio", "snap_pac", "RIO_MM1", ""] {
            let mut value = selector();
            value["model"] = json!(model);
            rejected(&value);
        }
        for resource in ["analog", "digital", "analog_output", "bool"] {
            let mut value = selector();
            value["channels"][0]["resource"] = json!(resource);
            rejected(&value);
        }
    }

    #[test]
    fn channel_count_names_and_physical_pairs_are_bounded_and_unique() {
        let mut value = selector();
        value["channels"] = json!([]);
        rejected(&value);
        let channel = selector()["channels"][0].clone();
        let mut value = selector();
        value["channels"] = json!([channel.clone(), channel]);
        rejected(&value);
        let mut renamed = channel.clone();
        renamed["name"] = json!("other");
        let mut value = selector();
        value["channels"] = json!([channel, renamed]);
        rejected(&value);
        for name in [String::new(), "\n".into(), "x".repeat(1025)] {
            let mut value = selector();
            value["channels"][0]["name"] = json!(name);
            rejected(&value);
        }
        let mut value = selector();
        value["model"] = json!("epic_pr1");
        let channels = (0..65)
            .map(|index: u32| {
                json!({"name":format!("c{index}"),"module":index / 24,"channel":index % 24,
                    "resource":"digital_state","expected_module_id":0x5000_0011_u32,
                    "expected_channel_type":0x5000_0000_u32})
            })
            .collect::<Vec<_>>();
        value["channels"] = json!(channels.get(..64));
        serde_json::from_value::<Config>(value.clone()).unwrap();
        value["channels"] = json!(channels);
        rejected(&value);
    }

    #[test]
    fn requested_identity_must_also_pass_the_closed_table() {
        for (module, channel, resource, id, kind) in [
            (1, 0, "analog_eu", 0xF000_0022_u32, 0x6000_0015_u32),
            (0, 8, "analog_eu", 0xF000_0022, 0x6000_0015),
            (0, 0, "analog_eu", 0xF000_002C, 0x6000_0015),
            (0, 0, "analog_eu", 0xF000_0022, 0x9000_007B),
            (0, 0, "digital_state", 0xF000_0022, 0x6000_0015),
            (0, 4, "analog_eu", 0xF000_0022, 0x6000_0005),
        ] {
            let mut value = selector();
            value["channels"][0]["module"] = json!(module);
            value["channels"][0]["channel"] = json!(channel);
            value["channels"][0]["resource"] = json!(resource);
            value["channels"][0]["expected_module_id"] = json!(id);
            value["channels"][0]["expected_channel_type"] = json!(kind);
            rejected(&value);
        }
    }
}
