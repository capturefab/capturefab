//! Pure opaque `GenTL` identity parsing; never loads native code or probes a device.
use super::gendc;
use crate::types::CameraInfo;
use anyhow::{Context as _, Result, ensure};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
const PREFIX: &str = "gentl:";
const MAX_NAME: usize = 4096;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Selector {
    pub(crate) producer: PathBuf,
    pub(crate) interface: String,
    pub(crate) device: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) stream: Option<String>,
    #[serde(
        default,
        deserialize_with = "crate::config_number::optional_unsigned",
        skip_serializing_if = "Option::is_none"
    )]
    pub(crate) part: Option<u32>,
    /// The expected PFNC code of an explicitly selected measurement component.
    #[serde(
        default,
        deserialize_with = "crate::config_number::optional_unsigned",
        skip_serializing_if = "Option::is_none"
    )]
    pub(crate) part_format: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) gendc: Option<gendc::Selection>,
}
impl Selector {
    pub(crate) fn parse(value: &str) -> Result<Self> {
        let value = value
            .strip_prefix(PREFIX)
            .context("invalid GenTL camera selector")?;
        ensure!(
            value.len() <= 32 * 1024,
            "GenTL selector exceeds size limit"
        );
        let selector: Self = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(value)?)
            .context("invalid GenTL camera selector")?;
        selector.validate()?;
        Ok(selector)
    }
    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(
            self.producer.is_absolute() && !self.producer.to_string_lossy().contains('\0'),
            "GenTL producer path must be absolute and contain no NUL"
        );
        for id in [&self.interface, &self.device]
            .into_iter()
            .chain(self.stream.as_ref())
        {
            ensure!(
                !id.is_empty() && id.len() <= MAX_NAME && !id.contains('\0'),
                "invalid GenTL module identifier"
            );
        }
        ensure!(
            self.part.is_none_or(|part| part < 32),
            "GenTL part index must be below 32"
        );
        ensure!(
            self.part_format.is_none_or(
                |format| self.part.is_some() && crate::frame::is_measurement_format(format)
            ),
            "GenTL measurement selector requires a part index and a supported format"
        );
        if let Some(selection) = self.gendc {
            ensure!(
                self.part.is_none(),
                "GenDC and GenTL multipart selections are mutually exclusive"
            );
            selection.validate()?;
        }
        Ok(())
    }

    pub(crate) fn measurement_selected(&self) -> bool {
        self.part_format.is_some()
            || self
                .gendc
                .is_some_and(|selection| crate::frame::is_measurement_format(selection.pfnc_code))
    }

    pub(crate) fn encode(&self) -> Result<String> {
        self.validate()?;
        let encoded = URL_SAFE_NO_PAD.encode(serde_json::to_vec(self)?);
        ensure!(
            encoded.len() <= 32 * 1024,
            "GenTL selector exceeds size limit"
        );
        Ok(format!("{PREFIX}{encoded}"))
    }

    pub(crate) fn image_selectors(
        &self,
        layout: &super::multipart::Layout,
        include: impl Fn(&super::multipart::Part, u32) -> bool,
    ) -> (Vec<serde_json::Value>, usize) {
        let mut choices = Vec::new();
        let mut truncated = 0;
        let mut bytes = 2usize; // JSON array brackets.
        for part in &layout.parts {
            let Ok((_, _, format)) = layout.image_shape(part) else {
                continue;
            };
            if !include(part, format) {
                continue;
            }
            let mut selected = self.clone();
            selected.gendc = layout
                .gendc
                .as_ref()
                .and_then(|container| container.selection(part));
            selected.part = selected.gendc.is_none().then_some(part.index);
            selected.part_format = selected.part.and_then(|_| part.measurement_format());
            let Ok(id) = selected.encode() else {
                truncated += 1;
                continue;
            };
            let choice = selected.gendc.map_or_else(
                || serde_json::json!({"part":part.index,"camera":id}),
                |selection| serde_json::json!({"gendc":selection,"camera":id}),
            );
            let Ok(encoded) = serde_json::to_vec(&choice) else {
                truncated += 1;
                continue;
            };
            let next = bytes
                .saturating_add(encoded.len())
                .saturating_add(usize::from(!choices.is_empty()));
            if next > 128 * 1024 {
                truncated += 1;
                continue;
            }
            choices.push(choice);
            bytes = next;
        }
        (choices, truncated)
    }
}

/// Parse an opaque selector without loading or probing a producer.
pub(crate) fn validate_selector_shape(selector: &str) -> Result<()> {
    Selector::parse(selector).map(|_| ())
}

/// A stable worker identity. Device probing happens only through `connect` in
/// its isolated helper, after the coordinator has selected the camera worker.
pub(crate) fn identify(selector: &str) -> Result<CameraInfo> {
    let parsed = Selector::parse(selector)?;
    Ok(CameraInfo {
        id: selector.into(),
        transport: crate::types::Transport::GenTl,
        vendor: "GenTL".into(),
        model: parsed.device.clone(),
        serial: parsed.device,
        address: Some(parsed.producer.display().to_string()),
        profile: crate::camera_profile::CameraProfile::default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encoded(value: &serde_json::Value) -> String {
        format!(
            "gentl:{}",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(value).unwrap())
        )
    }

    #[test]
    fn multipart_part_indices_accept_exact_numeric_notation_without_probing() {
        let producer = std::env::current_dir().unwrap().join("absent-producer.cti");
        for token in ["1", "1.0", "1e0"] {
            let raw = format!(
                r#"{{"producer":{},"interface":"i","device":"d","part":{token}}}"#,
                serde_json::to_string(&producer).unwrap()
            );
            let selector = format!("gentl:{}", URL_SAFE_NO_PAD.encode(raw));
            assert_eq!(Selector::parse(&selector).unwrap().part, Some(1));
            identify(&selector).unwrap();
        }
        for token in ["32", "1.00000000000000000001", "-1", "4294967296"] {
            let raw = format!(
                r#"{{"producer":{},"interface":"i","device":"d","part":{token}}}"#,
                serde_json::to_string(&producer).unwrap()
            );
            Selector::parse(&format!("gentl:{}", URL_SAFE_NO_PAD.encode(raw))).unwrap_err();
        }
        let raw = format!(
            r#"{{"producer":{},"interface":"i","device":"d","part":1,"part":2}}"#,
            serde_json::to_string(&producer).unwrap()
        );
        Selector::parse(&format!("gentl:{}", URL_SAFE_NO_PAD.encode(raw))).unwrap_err();
    }
    #[test]
    fn scalar_map_selector_binds_the_part_to_a_supported_exact_format() {
        let producer = std::env::current_dir().unwrap().join("absent-producer.cti");
        let mut value = serde_json::json!({
            "producer": producer, "interface":"i", "device":"d", "part":0, "part_format":0x0110_00b8_u32,
        });
        let id = encoded(&value);
        assert_eq!(Selector::parse(&id).unwrap().part_format, Some(0x0110_00b8));
        assert_eq!(identify(&id).unwrap().id, id);
        value["part"] = serde_json::Value::Null;
        Selector::parse(&encoded(&value)).unwrap_err();
        value["part"] = serde_json::json!(0);
        value["part_format"] = serde_json::json!(crate::frame::COORD3D_ABC32F);
        assert_eq!(
            Selector::parse(&encoded(&value)).unwrap().part_format,
            Some(crate::frame::COORD3D_ABC32F)
        );
        value["part_format"] = serde_json::json!(0x0260_00c1_u32);
        Selector::parse(&encoded(&value)).unwrap_err();

        let raw = format!(
            r#"{{"producer":{},"interface":"i","device":"d","part":0,"part_format":{}.0}}"#,
            serde_json::to_string(&producer).unwrap(),
            0x0110_00c7_u32,
        );
        let id = format!("gentl:{}", URL_SAFE_NO_PAD.encode(raw));
        assert_eq!(Selector::parse(&id).unwrap().part_format, Some(0x0110_00c7));
    }

    #[test]
    fn opaque_identity_is_metadata_only_and_does_not_require_a_producer_to_exist() {
        let producer = std::env::current_dir()
            .unwrap()
            .join("not-an-installed-producer.cti");
        let selector = encoded(
            &serde_json::json!({"producer":producer, "interface":"interface/0;?", "device":"camera:0/test"}),
        );
        let info = identify(&selector).unwrap();
        assert_eq!(info.id, selector);
        assert_eq!(info.serial, "camera:0/test");
        assert_eq!(info.transport, crate::types::Transport::GenTl);
    }

    #[test]
    fn opaque_identity_rejects_ambiguous_or_unbounded_shapes() {
        let producer = std::env::current_dir().unwrap().join("producer.cti");
        for value in [
            serde_json::json!({"producer":"relative.cti", "interface":"i", "device":"d"}),
            serde_json::json!({"producer":producer, "interface":"", "device":"d"}),
            serde_json::json!({"producer":producer, "interface":"i", "device":"d\u{0}"}),
            serde_json::json!({"producer":producer, "interface":"i", "device":"d", "stream":""}),
            serde_json::json!({"producer":producer, "interface":"i", "device":"d", "other":1}),
            serde_json::json!({"producer":producer, "interface":"i", "device":"d".repeat(MAX_NAME + 1)}),
        ] {
            assert!(
                validate_selector_shape(&encoded(&value)).is_err(),
                "{value}"
            );
        }
        assert!(validate_selector_shape(&format!("gentl:{}", "a".repeat(32 * 1024 + 1))).is_err());
    }
}
