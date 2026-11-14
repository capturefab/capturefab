//! Frozen manufacturer metadata, without synthesized calibration or live requests.
use super::{
    Config, StreamKind,
    packet::{Format, Integrity, Profile},
};
use crate::types::Usage;
use anyhow::{Context as _, Result, ensure};
use serde::de::IntoDeserializer as _;
use serde_json::{Value, json};
use std::{fs::File, io::Read as _, path::Path};

pub(super) const MAX_METADATA: usize = 1024 * 1024;
#[derive(Debug)]
pub(super) struct Metadata {
    pub raw: Vec<u8>,
    pub format: Format,
    pub summary: Value,
    pub other: Option<Format>,
    pub other_standard: Option<(u64, u32, u16, Integrity)>,
}
fn invalid(message: &str) -> anyhow::Error {
    Usage(format!("Ouster metadata: {message}")).into()
}
fn section<'a>(root: &'a Value, name: &str, alias: &str) -> Result<&'a Value> {
    if let (Some(value), Some(other)) = (root.get(name), root.get(alias)) {
        ensure!(
            value == other,
            invalid("contradictory metadata section aliases")
        );
    }
    let value = root.get(name).or_else(|| root.get(alias)).unwrap_or(root);
    ensure!(
        value.is_object(),
        invalid("metadata section must be an object")
    );
    Ok(value)
}
fn field<'a>(section: &'a Value, root: &'a Value, key: &str) -> Result<Option<&'a Value>> {
    if let (Some(value), Some(other)) = (section.get(key), root.get(key)) {
        ensure!(value == other, invalid("contradictory metadata fields"));
    }
    Ok(section.get(key).or_else(|| root.get(key)))
}
fn text<'a>(section: &'a Value, root: &'a Value, key: &str) -> Result<&'a str> {
    field(section, root, key)?
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= 1024)
        .ok_or_else(|| invalid(&format!("missing or invalid {key}")))
}
fn integer(value: &Value) -> Result<u64> {
    crate::config_number::unsigned(value.clone().into_deserializer())
        .map_err(|error| invalid(&format!("expected an exact unsigned integer: {error}")))
}
fn number_fact(
    section: &Value,
    root: &Value,
    key: &str,
    declared: Option<usize>,
    derived: Option<usize>,
    facts: &mut Value,
) -> Result<usize> {
    let present = field(section, root, key)?
        .map(integer)
        .transpose()?
        .map(usize::try_from)
        .transpose()?;
    if let (Some(actual), Some(declared)) = (present, declared) {
        ensure!(
            actual == declared,
            invalid("configured dimensions contradict metadata")
        );
    }
    facts[key] = json!(if present.is_some() {
        "metadata"
    } else if declared.is_some() {
        "configured"
    } else {
        "wire_definition"
    });
    present
        .or(declared)
        .or(derived)
        .ok_or_else(|| invalid(&format!("missing {key}; supply an explicit declaration")))
}
fn text_fact<'a>(
    present: Option<&'a Value>,
    declared: Option<&'a str>,
    key: &str,
    facts: &mut Value,
) -> Result<&'a str> {
    let present = present
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| invalid("format fields must be strings"))
        })
        .transpose()?;
    if let (Some(actual), Some(declared)) = (present, declared) {
        ensure!(
            actual == declared,
            invalid("configured format contradicts metadata")
        );
    }
    facts[key] = json!(if present.is_some() {
        "metadata"
    } else {
        "configured"
    });
    present
        .or(declared)
        .ok_or_else(|| invalid(&format!("missing {key}; supply an explicit declaration")))
}
fn finite(value: &Value) -> bool {
    match value {
        Value::Number(value) => value.as_f64().is_some_and(f64::is_finite),
        Value::Array(values) => values.iter().all(finite),
        Value::Object(values) => values.values().all(finite),
        _ => true,
    }
}
fn version(build: &str) -> Option<(u32, u32, u32)> {
    fn decimal(value: &str) -> Option<u32> {
        if value.is_empty()
            || !value.bytes().all(|byte| byte.is_ascii_digit())
            || (value.len() > 1 && value.starts_with('0'))
        {
            return None;
        }
        value.parse().ok()
    }
    let build = build.strip_prefix('v').unwrap_or(build);
    let (core, suffix) = build
        .split_once('-')
        .map_or((build, None), |(core, suffix)| (core, Some(suffix)));
    let mut parts = core.split('.');
    let (major, minor, patch) = (
        decimal(parts.next()?)?,
        decimal(parts.next()?)?,
        decimal(parts.next()?)?,
    );
    if parts.next().is_some() {
        return None;
    }
    if let Some(suffix) = suffix {
        // Observed vendor fixture: v2.2.0-rc.2-17-gbc0879e. No arbitrary
        // suffix, extra dotted component or opaque hash can weaken CRC.
        let rest = suffix.strip_prefix("rc.")?;
        let mut fields = rest.split('-');
        decimal(fields.next()?)?;
        if let Some(commits) = fields.next() {
            decimal(commits)?;
            let hash = fields.next()?.strip_prefix('g')?;
            if !(7..=40).contains(&hash.len()) || !hash.bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                return None;
            }
        }
        if fields.next().is_some() {
            return None;
        }
    }
    Some((major, minor, patch))
}
fn integrity(build: &str) -> (Integrity, bool) {
    match version(build) {
        Some((2, 2..=4, _) | (3, 0, _)) => (Integrity::PreCrcReserved, false),
        Some((2, 5.., _) | (3, 1.., _) | (4, _, _)) => (Integrity::Required, true),
        _ => (Integrity::Required, false),
    }
}
fn calibration(root: &Value, height: usize) -> bool {
    let angles = root.get("beam_intrinsics").unwrap_or(root);
    let arrays = ["beam_altitude_angles", "beam_azimuth_angles"]
        .into_iter()
        .all(|name| {
            angles
                .get(name)
                .and_then(Value::as_array)
                .is_some_and(|array| {
                    array.len() == height
                        && array
                            .iter()
                            .all(|value| value.as_f64().is_some_and(f64::is_finite))
                })
        });
    let transform = root
        .get("lidar_intrinsics")
        .unwrap_or(root)
        .get("lidar_to_sensor_transform")
        .and_then(Value::as_array)
        .is_some_and(|array| {
            array.len() == 16
                && array
                    .iter()
                    .all(|value| value.as_f64().is_some_and(f64::is_finite))
        });
    height > 0 && arrays && transform
}
impl Metadata {
    pub(super) fn read(path: &Path, config: &Config) -> Result<Self> {
        let mut raw = Vec::new();
        File::open(path)
            .context("open Ouster metadata")?
            .take(u64::try_from(MAX_METADATA + 1)?)
            .read_to_end(&mut raw)?;
        Self::parse(raw, config)
    }
    pub(super) fn parse(raw: Vec<u8>, config: &Config) -> Result<Self> {
        let stream = config.stream;
        ensure!(
            !raw.is_empty() && raw.len() <= MAX_METADATA,
            invalid("JSON must be 1..1048576 bytes")
        );
        let mut input = serde_json::Deserializer::from_slice(&raw);
        let root = crate::session::strict_json_value(&mut input)
            .map_err(|error| invalid(&error.to_string()))?;
        input.end().map_err(|error| invalid(&error.to_string()))?;
        ensure!(
            root.is_object() && finite(&root),
            invalid("expected finite JSON object")
        );
        let sensor = section(&root, "sensor_info", "sensor_info")?;
        let data = section(&root, "lidar_data_format", "data_format")?;
        let model = text(sensor, &root, "prod_line")?;
        ensure!(
            ["OS-0-", "OS-1-", "OS-2-", "OS0-", "OS1-", "OS2-"]
                .iter()
                .any(|prefix| model.starts_with(prefix)),
            invalid("requires an explicit OS0/OS1/OS2 model")
        );
        let serial_text = text(sensor, &root, "prod_sn")?;
        let serial = serial_text
            .parse::<u64>()
            .ok()
            .filter(|serial| *serial < (1_u64 << 40))
            .filter(|_| serial_text.bytes().all(|byte| byte.is_ascii_digit()))
            .ok_or_else(|| invalid("serial must be a decimal 40-bit value"))?;
        let build = text(sensor, &root, "build_rev")?;
        let (modern_integrity, header_features) = integrity(build);
        let config_params = root.get("config_params").unwrap_or(&root);
        ensure!(
            config_params.is_object(),
            invalid("config_params must be an object")
        );
        let mut facts = json!({});
        let profile_value = match stream {
            StreamKind::Lidar => field(data, &root, "udp_profile_lidar")?,
            StreamKind::Imu => field(
                section(&root, "imu_data_format", "imu_data_format")?,
                data,
                "udp_profile_imu",
            )?,
        };
        let profile_name = text_fact(
            profile_value,
            config.profile.as_deref(),
            "profile",
            &mut facts,
        )?;
        let profile = match (stream, profile_name) {
            (StreamKind::Lidar, "RNG19_RFL8_SIG16_NIR16") => Profile::ModernSingle,
            (StreamKind::Lidar, "RNG19_RFL8_SIG16_NIR16_DUAL") => Profile::ModernDual,
            (StreamKind::Lidar, "RNG15_RFL8_NIR8") => Profile::ModernLow,
            (StreamKind::Lidar, "RNG15_RFL8_NIR8_DUAL") => Profile::ModernLowDual,
            (StreamKind::Lidar, "LEGACY") => Profile::LegacyLidar,
            (StreamKind::Imu, "LEGACY") => Profile::LegacyImu,
            (StreamKind::Imu, "ACCEL32_GYRO32_NMEA") => Profile::SynchronousImu,
            _ => return Err(invalid("unsupported explicit stream/profile")),
        };
        let configured_profile = match stream {
            StreamKind::Lidar => "udp_profile_lidar",
            StreamKind::Imu => "udp_profile_imu",
        };
        for section in [
            &root,
            data,
            section(&root, "imu_data_format", "imu_data_format")?,
            config_params,
        ] {
            if let Some(value) = section.get(configured_profile) {
                ensure!(
                    value.as_str() == Some(profile_name),
                    invalid("selected profile aliases contradict each other")
                );
            }
        }
        let modern = !matches!(profile, Profile::LegacyLidar | Profile::LegacyImu);
        if modern {
            let header = text_fact(
                field(data, &root, "header_type")?,
                config.header_type.as_deref(),
                "header_type",
                &mut facts,
            )?;
            ensure!(
                header == "STANDARD",
                invalid("only explicit STANDARD header is supported")
            );
            if let Some(other) = root
                .get("config_params")
                .and_then(|params| params.get("header_type"))
            {
                ensure!(
                    other.as_str() == Some("STANDARD"),
                    invalid("header_type differs in config_params")
                );
            }
        }
        let mut format = Format {
            profile,
            pixels_per_column: 0,
            columns_per_packet: 0,
            columns_per_frame: 0,
            serial: None,
            init_id: None,
            window: false,
            imu_measurements_per_packet: 0,
            integrity: if modern {
                modern_integrity
            } else {
                Integrity::NoCrcInFormat
            },
            header_features: modern && header_features,
        };
        if !profile.is_imu() {
            format.pixels_per_column = number_fact(
                data,
                &root,
                "pixels_per_column",
                config.pixels_per_column,
                None,
                &mut facts,
            )?;
            format.columns_per_packet = number_fact(
                data,
                &root,
                "columns_per_packet",
                config.columns_per_packet,
                matches!(profile, Profile::LegacyLidar).then_some(16),
                &mut facts,
            )?;
            format.columns_per_frame = number_fact(
                data,
                &root,
                "columns_per_frame",
                config.columns_per_frame,
                None,
                &mut facts,
            )?;
            if let Some(value) = field(data, &root, "column_window")? {
                let window = value
                    .as_array()
                    .filter(|window| window.len() == 2)
                    .ok_or_else(|| invalid("column_window requires two indices"))?;
                ensure!(
                    integer(&window[0])? < u64::try_from(format.columns_per_frame)?
                        && integer(&window[1])? < u64::try_from(format.columns_per_frame)?,
                    invalid("column_window exceeds frame width")
                );
            }
            for (name, chosen) in [
                ("pixels_per_column", format.pixels_per_column),
                ("columns_per_packet", format.columns_per_packet),
                ("columns_per_frame", format.columns_per_frame),
            ] {
                if let Some(value) = config_params.get(name) {
                    ensure!(
                        integer(value)? == u64::try_from(chosen)?,
                        invalid("format dimensions differ in config_params")
                    );
                }
            }
            format.window = matches!(
                profile,
                Profile::ModernSingle | Profile::ModernDual | Profile::ModernLowDual
            ) && matches!(version(build), Some((3, 2.., _) | (4, _, _)));
        } else if matches!(profile, Profile::SynchronousImu) {
            format.imu_measurements_per_packet = number_fact(
                section(&root, "imu_data_format", "imu_data_format")?,
                &root,
                "imu_measurements_per_packet",
                None,
                None,
                &mut facts,
            )?;
        }
        if modern {
            format.serial = Some(serial);
            format.init_id = Some(u32::try_from(integer(
                field(sensor, &root, "initialization_id")?
                    .ok_or_else(|| invalid("missing initialization_id"))?,
            )?)?);
        }
        super::packet::expected_packet_bytes(&format)
            .map_err(|error| invalid(&format!("invalid packet format: {error:#}")))?;
        ensure!(
            !profile.is_imu()
                || (config.pixels_per_column.is_none()
                    && config.columns_per_packet.is_none()
                    && config.columns_per_frame.is_none()),
            invalid("IMU source does not accept lidar dimension declarations")
        );
        ensure!(
            super::packet::decoded_json_bound(&format)? <= super::MAX_VALUE_JSON,
            invalid("configured decoded packet exceeds the 1 MiB admission bound")
        );
        let mut summary = json!({"model":model,"serial":serial_text,"build_rev":build,
            "profile":profile.name(),"stream":stream,"clock_basis":"unspecified",
            "calibration_supplied":calibration(&root, format.pixels_per_column),"format_sources":facts,"column_window_known":field(data, &root, "column_window")?.is_some(),"integrity":format.integrity.name(),"integrity_source":if modern && version(build).is_some() {"literal_build_rev"} else if modern {"conservative_crc_required"} else {"wire_definition"}});
        if let Some(value) = config_params.get("timestamp_mode") {
            let clock = value
                .as_str()
                .filter(|clock| clock.len() <= 1024)
                .ok_or_else(|| invalid("timestamp_mode must be a bounded string"))?;
            summary["timestamp_mode"] = json!(clock);
        }
        let other_name = match stream {
            StreamKind::Lidar => field(
                section(&root, "imu_data_format", "imu_data_format")?,
                data,
                "udp_profile_imu",
            )?
            .and_then(Value::as_str),
            StreamKind::Imu => field(data, &root, "udp_profile_lidar")?.and_then(Value::as_str),
        };
        let other = if other_name == Some("LEGACY") {
            let candidate = if stream == StreamKind::Lidar {
                Some(Format {
                    profile: Profile::LegacyImu,
                    pixels_per_column: 0,
                    columns_per_packet: 0,
                    columns_per_frame: 0,
                    serial: None,
                    init_id: None,
                    window: false,
                    imu_measurements_per_packet: 0,
                    integrity: Integrity::NoCrcInFormat,
                    header_features: false,
                })
            } else {
                let dimensions = [
                    "pixels_per_column",
                    "columns_per_packet",
                    "columns_per_frame",
                ]
                .map(|name| {
                    field(data, &root, name)
                        .ok()
                        .flatten()
                        .and_then(|value| integer(value).ok())
                        .and_then(|value| usize::try_from(value).ok())
                });
                match dimensions {
                    [Some(height), columns, Some(width)] => Some(Format {
                        profile: Profile::LegacyLidar,
                        pixels_per_column: height,
                        columns_per_packet: columns.unwrap_or(16),
                        columns_per_frame: width,
                        serial: None,
                        init_id: None,
                        window: false,
                        imu_measurements_per_packet: 0,
                        integrity: Integrity::NoCrcInFormat,
                        header_features: false,
                    }),
                    _ => None,
                }
            };
            candidate.filter(|candidate| super::packet::expected_packet_bytes(candidate).is_ok())
        } else {
            None
        };
        let declared_header = field(data, &root, "header_type")?
            .and_then(Value::as_str)
            .or(config.header_type.as_deref());
        let mut other_types = if other_name.is_some_and(|name| name != "LEGACY") {
            1_u16 << if stream == StreamKind::Lidar { 2 } else { 1 }
        } else {
            0
        };
        let zone_enabled = config_params
            .get("udp_port_zm")
            .map(integer)
            .transpose()?
            .is_some_and(|port| (1..=65_535).contains(&port))
            && root
                .get("zone_set")
                .and_then(Value::as_str)
                .is_some_and(|set| !set.is_empty());
        if zone_enabled && modern_integrity == Integrity::Required {
            other_types |= 1 << 3;
        }
        let other_standard = if other_types != 0 && declared_header == Some("STANDARD") {
            field(sensor, &root, "initialization_id")?
                .map(integer)
                .transpose()?
                .and_then(|init| u32::try_from(init).ok())
                .filter(|init| *init < (1_u32 << 24))
                .map(|init| (serial, init, other_types, modern_integrity))
        } else {
            None
        };
        summary["unselected_filter_validation"] = json!(if other_standard.is_some() {
            if matches!(modern_integrity, Integrity::Required) {
                "header_identity_crc"
            } else {
                "header_identity_pre_crc_framing"
            }
        } else if other.is_some() {
            "declared_legacy_framing_and_decode"
        } else {
            "unavailable"
        });
        summary["legacy_framing_has_no_crc_or_type"] =
            json!(matches!(profile, Profile::LegacyLidar | Profile::LegacyImu) || other.is_some());
        Ok(Self {
            raw,
            format,
            summary,
            other,
            other_standard,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn settings(stream: &str) -> Config {
        serde_json::from_value(json!({"listen":"127.0.0.1:7502","source_ip":"127.0.0.1","stream":stream,"metadata":"sensor.json","output":"raw.ouster","max_bytes":10_000_000})).unwrap()
    }
    fn raw(profile: &str, build: &str) -> Value {
        json!({"sensor_info":{"prod_line":"OS-1-32","prod_sn":"123456789012","build_rev":build,"initialization_id":42},
            "lidar_data_format":{"udp_profile_lidar":profile,"udp_profile_imu":"LEGACY","header_type":"STANDARD","pixels_per_column":32,"columns_per_packet":16,"columns_per_frame":512,"column_window":[0,511]},
            "config_params":{"timestamp_mode":"TIME_FROM_INTERNAL_OSC"},"future_metadata":{"uninterpreted":true}})
    }
    fn parse(value: &Value, config: &Config) -> Metadata {
        Metadata::parse(serde_json::to_vec(value).unwrap(), config).unwrap()
    }
    #[test]
    fn only_recognized_literal_build_versions_weaken_crc_and_window_requires_supported_layout() {
        let settings = settings("lidar");
        for build in ["v2.2.0", "v2.4.1", "v3.0.1", "v2.2.0-rc.2-17-gbc0879e"] {
            let metadata = parse(&raw("RNG19_RFL8_SIG16_NIR16_DUAL", build), &settings);
            assert_eq!(metadata.format.integrity, Integrity::PreCrcReserved);
            assert!(!metadata.format.header_features);
        }
        for build in [
            "v2.4.0.99",
            "v2.4.0-anything",
            "v2.4.0-rc.1-extra",
            "v2.4.0-rc.1-5-gbad",
            "035522cb7ffa0b1f8c4a",
            "v02.4.0",
        ] {
            let mut value = raw("RNG19_RFL8_SIG16_NIR16", build);
            value["sensor_info"]["image_rev"] = json!("ousteros-image-v2.2.0");
            let metadata = parse(&value, &settings);
            assert_eq!(metadata.format.integrity, Integrity::Required);
            assert!(!metadata.format.header_features);
            assert!(!metadata.format.window);
        }
        for (profile, window) in [
            ("RNG15_RFL8_NIR8", false),
            ("RNG15_RFL8_NIR8_DUAL", true),
            ("RNG19_RFL8_SIG16_NIR16", true),
        ] {
            let metadata = parse(&raw(profile, "v3.2.0"), &settings);
            assert_eq!(metadata.format.window, window);
            assert_eq!(metadata.format.integrity, Integrity::Required);
        }
    }
    #[test]
    fn explicit_declarations_admit_older_missing_facts_without_sdk_defaults_or_calibration() {
        let mut settings = settings("lidar");
        settings.profile = Some("LEGACY".into());
        settings.pixels_per_column = Some(32);
        settings.columns_per_frame = Some(512);
        let mut value = raw("LEGACY", "v2.2.0");
        for key in [
            "udp_profile_lidar",
            "pixels_per_column",
            "columns_per_packet",
            "columns_per_frame",
            "column_window",
            "header_type",
        ] {
            value["lidar_data_format"]
                .as_object_mut()
                .unwrap()
                .remove(key);
        }
        let original = serde_json::to_vec_pretty(&value).unwrap();
        let metadata = Metadata::parse(original.clone(), &settings).unwrap();
        assert_eq!(metadata.raw, original);
        assert_eq!(metadata.format.columns_per_packet, 16);
        assert_eq!(
            metadata.summary["format_sources"]["columns_per_packet"],
            "wire_definition"
        );
        assert_eq!(metadata.summary["format_sources"]["profile"], "configured");
        assert_eq!(metadata.summary["calibration_supplied"], false);
        assert_eq!(metadata.summary["column_window_known"], false);
        settings.pixels_per_column = None;
        assert!(
            Metadata::parse(original, &settings)
                .unwrap_err()
                .to_string()
                .contains("pixels_per_column")
        );
    }
    #[test]
    fn contradictory_duplicate_and_unbounded_metadata_fails_before_transport() {
        let settings = settings("lidar");
        let value = raw("RNG19_RFL8_SIG16_NIR16", "v3.2.0");
        let mut contradiction = value.clone();
        contradiction["data_format"] = json!({"pixels_per_column":64});
        assert!(
            Metadata::parse(serde_json::to_vec(&contradiction).unwrap(), &settings)
                .unwrap_err()
                .to_string()
                .contains("contradictory")
        );
        let duplicate = br#"{"sensor_info":{},"sensor_info":{}}"#;
        assert!(
            Metadata::parse(duplicate.to_vec(), &settings)
                .unwrap_err()
                .to_string()
                .contains("duplicate")
        );
        assert!(
            Metadata::parse(vec![b' '; MAX_METADATA + 1], &settings)
                .unwrap_err()
                .to_string()
                .contains("1048576")
        );
        let mut alias = value.clone();
        alias["udp_profile_lidar"] = json!("LEGACY");
        assert!(
            Metadata::parse(serde_json::to_vec(&alias).unwrap(), &settings)
                .unwrap_err()
                .to_string()
                .contains("contradict")
        );
        let mut missing_header = value;
        missing_header["lidar_data_format"]
            .as_object_mut()
            .unwrap()
            .remove("header_type");
        assert!(
            Metadata::parse(serde_json::to_vec(&missing_header).unwrap(), &settings)
                .unwrap_err()
                .to_string()
                .contains("header_type")
        );
        let mut declared = settings;
        declared.header_type = Some("STANDARD".into());
        let metadata = parse(&missing_header, &declared);
        assert_eq!(
            metadata.summary["format_sources"]["header_type"],
            "configured"
        );
    }
    #[test]
    fn synchronous_imu_does_not_require_a_supported_lidar_body_or_synthesize_units() {
        let settings = settings("imu");
        let mut value = raw("RNG19_RFL8_SIG16_NIR16_ZONE16", "opaquehash");
        value["imu_data_format"] = json!({"udp_profile_imu":"ACCEL32_GYRO32_NMEA","imu_measurements_per_packet":8,"accel_fsr":"EXTENDED","gyro_fsr":"EXTENDED"});
        value["lidar_data_format"]["udp_profile_imu"] = json!("ACCEL32_GYRO32_NMEA");
        let metadata = parse(&value, &settings);
        assert_eq!(metadata.format.profile, Profile::SynchronousImu);
        assert_eq!(metadata.format.imu_measurements_per_packet, 8);
        assert_eq!(metadata.format.integrity, Integrity::Required);
        assert!(!metadata.format.header_features);
        assert!(metadata.summary.get("acceleration_unit").is_none());
        value["imu_data_format"]["imu_measurements_per_packet"] = json!(1815);
        assert!(
            Metadata::parse(serde_json::to_vec(&value).unwrap(), &settings)
                .unwrap_err()
                .to_string()
                .contains("admission bound")
        );
    }
}
