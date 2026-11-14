//! Native, unfiltered packet fields for explicit `RoboSense` model/stream views.
//!
//! Primary manuals: Helios-16P English v1.0.1 / Chinese v1.0.2 and RS16 v4.3.3:
//! <https://cdn.robosense.cn/20220629181832_42540.pdf>
//! <https://cdn.robosense.cn/20220923161259_62013.pdf>
//! <https://cdn.robosense.cn/20200723161607_95470.pdf>
//! Raw structures independently checked against official `rs_driver` at
//! `897b14d3bdb6186a75df27ba51b65b5bd5557723`; no SDK source is copied.
use crate::types::{Categorized, ErrorKind, Usage};
use anyhow::Result;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::io::{self, Write};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Profile {
    Helios16pMsop,
    Helios16pDifop,
    Rs16Msop,
    Rs16Difop,
}
impl Profile {
    pub(super) const fn name(self) -> &'static str {
        match self {
            Self::Helios16pMsop => "helios16p_msop",
            Self::Helios16pDifop => "helios16p_difop",
            Self::Rs16Msop => "rs16_msop",
            Self::Rs16Difop => "rs16_difop",
        }
    }
    pub(super) const fn is_msop(self) -> bool {
        matches!(self, Self::Helios16pMsop | Self::Rs16Msop)
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) struct Format {
    pub profile: Profile,
    pub rs16_distance_unit_um: Option<u16>,
}

pub(super) const fn expected_packet_bytes(_format: Format) -> usize {
    1248
}

/// Conservative bounds include dense integer arrays and exact raw byte regions.
pub(super) const fn decoded_json_bound(format: Format) -> usize {
    match format.profile {
        Profile::Helios16pMsop | Profile::Rs16Msop => 128 * 1024,
        Profile::Helios16pDifop | Profile::Rs16Difop => 16 * 1024,
    }
}

#[derive(Debug)]
pub(super) struct Decoded {
    pub value: Value,
    pub elements: usize,
}
fn malformed(message: &str) -> anyhow::Error {
    Categorized::error(ErrorKind::Incomplete, format!("RoboSense: {message}"))
}
fn unsupported(message: &str) -> anyhow::Error {
    Categorized::error(ErrorKind::Unsupported, format!("RoboSense: {message}"))
}
fn span(raw: &[u8], offset: usize, count: usize) -> Result<&[u8]> {
    let end = offset
        .checked_add(count)
        .ok_or_else(|| malformed("field offset overflow"))?;
    raw.get(offset..end)
        .ok_or_else(|| malformed("truncated field"))
}
fn bytes<const N: usize>(raw: &[u8], offset: usize) -> Result<[u8; N]> {
    span(raw, offset, N)?
        .try_into()
        .map_err(|_| malformed("incorrect field extent"))
}
fn u16_at(raw: &[u8], offset: usize) -> Result<u16> {
    Ok(u16::from_be_bytes(bytes(raw, offset)?))
}
fn u32_at(raw: &[u8], offset: usize) -> Result<u32> {
    Ok(u32::from_be_bytes(bytes(raw, offset)?))
}
fn u24_at(raw: &[u8], offset: usize) -> Result<u32> {
    let [a, b, c] = bytes(raw, offset)?;
    Ok(u32::from_be_bytes([0, a, b, c]))
}
fn u48_at(raw: &[u8], offset: usize) -> Result<u64> {
    let [a, b, c, d, e, f] = bytes(raw, offset)?;
    Ok(u64::from_be_bytes([0, 0, a, b, c, d, e, f]))
}
fn encoded(raw: &[u8], offset: usize, count: usize) -> Result<String> {
    Ok(STANDARD.encode(span(raw, offset, count)?))
}

struct JsonSize {
    count: usize,
    limit: usize,
}
impl Write for JsonSize {
    fn write(&mut self, raw: &[u8]) -> io::Result<usize> {
        self.count = self
            .count
            .checked_add(raw.len())
            .filter(|count| *count <= self.limit)
            .ok_or_else(|| {
                io::Error::other("RoboSense native JSON exceeds its fixed schema bound")
            })?;
        Ok(raw.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) fn decode(raw: &[u8], format: Format) -> Result<Decoded> {
    if let Some(unit) = format.rs16_distance_unit_um
        && (format.profile != Profile::Rs16Msop || !matches!(unit, 5_000 | 10_000))
    {
        return Err(Usage(
            "RoboSense rs16_distance_unit_um is allowed only for rs16_msop and must be 5000 or 10000".into(),
        )
        .into());
    }
    if raw.len() != expected_packet_bytes(format) {
        return Err(malformed(
            "datagram length does not match the selected profile",
        ));
    }
    let value = if format.profile.is_msop() {
        msop(raw, format)?
    } else {
        difop(raw, format.profile)?
    };
    serde_json::to_writer(
        JsonSize {
            count: 0,
            limit: decoded_json_bound(format),
        },
        &value,
    )
    .map_err(|error| {
        Categorized::error(
            ErrorKind::OperationFailed,
            format!("RoboSense bounded native serialization failed: {error}"),
        )
    })?;
    Ok(Decoded {
        value,
        elements: if format.profile.is_msop() { 384 } else { 1 },
    })
}

fn msop(raw: &[u8], format: Format) -> Result<Value> {
    if bytes::<2>(raw, 1246)? != [0, 0xff] {
        return Err(malformed("MSOP tail marker is not 00FF"));
    }
    let mut value = json!({
        "profile": format.profile.name(),
        "packet_type": "msop",
        "header_base64": encoded(raw, 0, 42)?,
        "tail_reserved_base64": encoded(raw, 1242, 4)?,
        "tail_marker_raw": [0, 255],
        "checksum_status": "no_checksum_in_format",
    });
    let unit = match format.profile {
        Profile::Helios16pMsop => {
            let qualified = helios_header(raw, &mut value)?;
            let [flag] = bytes::<1>(raw, 17)?;
            value["resolution_flag_raw"] = json!(flag);
            value["clock"] = helios_clock(raw, 20, qualified)?;
            let unit = match flag {
                0 => Some(5000_u16),
                1 => Some(2_500),
                _ => None,
            };
            if let Some(unit) = unit {
                value["unit_status"] = json!("documented_header");
                value["unit_origin"] = json!("header");
                value["distance_unit_um"] = json!(unit);
            } else {
                value["unit_status"] = json!("unknown");
            }
            unit
        }
        Profile::Rs16Msop => {
            if bytes::<8>(raw, 0)? != [0x55, 0xaa, 5, 0x0a, 0x5a, 0xa5, 0x50, 0xa0] {
                return Err(malformed("RS16 MSOP header marker is incorrect"));
            }
            let [model] = bytes::<1>(raw, 30)?;
            if model != 1 {
                return Err(unsupported(
                    "RS16 MSOP model is outside the selected profile",
                ));
            }
            value["model_raw"] = json!(model);
            value["clock"] = rs16_clock(raw, 20)?;
            if let Some(unit) = format.rs16_distance_unit_um {
                value["unit_status"] = json!("configured_unverified");
                value["unit_origin"] = json!("configured");
                value["distance_unit_um"] = json!(unit);
            } else {
                value["unit_status"] = json!("unavailable_firmware_dependent");
            }
            format.rs16_distance_unit_um
        }
        Profile::Helios16pDifop | Profile::Rs16Difop => {
            return Err(unsupported("DIFOP profile cannot decode MSOP data"));
        }
    };
    let mut blocks = Vec::with_capacity(12);
    for index in 0..12 {
        let offset = 42 + index * 100;
        if bytes::<2>(raw, offset)? != [0xff, 0xee] {
            return Err(malformed("MSOP block marker is not FFEE"));
        }
        let azimuth = u16_at(raw, offset + 2)?;
        let mut distance = Vec::with_capacity(32);
        let mut reflectivity = Vec::with_capacity(32);
        for channel in 0..32 {
            let [high, low, reflection] = bytes::<3>(raw, offset + 4 + channel * 3)?;
            distance.push(u16::from_be_bytes([high, low]));
            reflectivity.push(reflection);
        }
        let scaled = unit.map(|unit| {
            distance
                .iter()
                .map(|raw| u32::from(*raw) * u32::from(unit))
                .collect::<Vec<_>>()
        });
        let mut block = json!({
            "index": index,
            "azimuth_raw": azimuth,
            "azimuth_hundredths_degree": azimuth,
            "azimuth_range_status": if azimuth < 36_000 { "within_nominal_rotation" } else { "unknown" },
            "distance_raw": distance,
            "reflectivity_raw": reflectivity,
        });
        if let Some(scaled) = scaled {
            block["distance_um"] = json!(scaled);
        }
        blocks.push(block);
    }
    value["blocks"] = json!(blocks);
    Ok(value)
}

fn helios_header(raw: &[u8], value: &mut Value) -> Result<bool> {
    if bytes::<4>(raw, 0)? != [0x55, 0xaa, 5, 0x5a] {
        return Err(malformed("Helios MSOP header marker is incorrect"));
    }
    let version = u16_at(raw, 4)?;
    if version != 1 {
        return Err(unsupported("Helios MSOP protocol version is not 0001"));
    }
    // These are zero-based manual table offsets. The SDK member named
    // lidar_type is at offset30; its name does not override the manual layout.
    let identity = bytes::<3>(raw, 30)?;
    let [_, family, model] = identity;
    let qualified = match (family, model) {
        (7, _) => {
            value["header_variant"] = json!("legacy_shared_07");
            value["header_identity_status"] = json!("legacy_shared_family");
            value["model_raw"] = json!(family);
            false
        }
        (6, 3) => {
            value["header_variant"] = json!("type_06_model_03");
            value["header_identity_status"] = json!("model_code_only");
            value["lidar_type_raw"] = json!(family);
            value["model_raw"] = json!(model);
            true
        }
        _ => {
            return Err(unsupported(
                "Helios MSOP type/model is outside the selected profile",
            ));
        }
    };
    value["protocol_version_raw"] = json!(version);
    value["identity_bytes_raw"] = json!(identity);
    value["top_board_packet_count_raw"] = json!(u32_at(raw, 8)?);
    value["bottom_board_packet_count_raw"] = json!(u32_at(raw, 12)?);
    Ok(qualified)
}

fn rs16_clock(raw: &[u8], offset: usize) -> Result<Value> {
    let calendar = bytes::<6>(raw, offset)?;
    let [year_byte, month, day, hour, minute, second] = calendar;
    let millis = u16_at(raw, offset + 6)?;
    let micros = u16_at(raw, offset + 8)?;
    let reserved = [
        u16::from(month & 0xf0),
        u16::from(day & 0xe0),
        u16::from(hour & 0xe0),
        u16::from(minute & 0xc0),
        u16::from(second & 0xc0),
        millis & 0xfc00,
        micros & 0xfc00,
    ];
    let clear = reserved.iter().all(|&bits| bits == 0);
    let [
        month_bits,
        day_bits,
        hour_bits,
        minute_bits,
        second_bits,
        millis_bits,
        micros_bits,
    ] = reserved;
    let year = 2000 + u16::from(year_byte);
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => 28 + u8::from(leap),
        _ => 0,
    };
    let valid = clear
        && day > 0
        && day <= days
        && hour < 24
        && minute < 60
        && second < 60
        && millis <= 999
        && micros <= 999;
    let status = if clear {
        if valid { "valid" } else { "invalid_value" }
    } else {
        "unknown_reserved_bits"
    };
    let mut value = json!({
        "raw_base64": encoded(raw, offset, 10)?,
        "calendar_fields_raw": calendar,
        "millisecond_word_raw": millis,
        "microsecond_word_raw": micros,
        "reserved_bits_raw": {
            "month":month_bits,"day":day_bits,"hour":hour_bits,
            "minute":minute_bits,"second":second_bits,
            "millisecond":millis_bits,"microsecond":micros_bits,
        },
        "reserved_bits_status":if clear { "zero" } else { "nonzero" },
        "calendar_status":status,
        "calendar_decoding": if valid { "decoded_unzoned" } else { "raw_only" },
        "synchronization_status": "unknown",
    });
    // Withhold the formatted calendar if any reserved bit is nonzero. The
    // original byte/word values remain available; never silently mask them
    // into an apparently valid civil instant or infer clock synchronization.
    if valid {
        let fraction = u32::from(millis) * 1000 + u32::from(micros);
        value["calendar_unzoned"] = json!(format!(
            "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{fraction:06}"
        ));
    }
    Ok(value)
}
fn helios_clock(raw: &[u8], offset: usize, qualified: bool) -> Result<Value> {
    let subsecond = u32_at(raw, offset + 6)?;
    let mut value = json!({
        "raw_base64": encoded(raw, offset, 10)?,
        "seconds_raw": u48_at(raw, offset)?,
        "subsecond_raw": subsecond,
        "synchronization_status": "unknown",
    });
    if qualified {
        value["subsecond_units_status"] = json!("documented_microseconds");
        value["subsecond_unit"] = json!("microseconds");
        value["subsecond_status"] = json!(if subsecond <= 999_999 {
            "valid"
        } else {
            "invalid_value"
        });
    } else {
        value["subsecond_units_status"] = json!("unresolved_vendor_documentation");
        value["header_documented_unit"] = json!("microseconds");
        value["conflicting_note_unit"] = json!("nanoseconds");
        value["sdk_interpreted_unit"] = json!("microseconds");
    }
    Ok(value)
}

fn difop(raw: &[u8], profile: Profile) -> Result<Value> {
    if bytes::<8>(raw, 0)? != [0xa5, 0xff, 0, 0x5a, 0x11, 0x11, 0x55, 0x55] {
        return Err(malformed("DIFOP header marker is incorrect"));
    }
    if bytes::<2>(raw, 1246)? != [0x0f, 0xf0] {
        return Err(malformed("DIFOP tail marker is not 0FF0"));
    }
    let mut value = json!({
        "profile": profile.name(),
        "packet_type": "difop",
        "checksum_status": "no_checksum_in_format",
        "data_base64": encoded(raw, 8, 1238)?,
        "tail_marker_raw": [15, 240],
        "motor_speed_rpm": u16_at(raw, 8)?,
        "network_base64": encoded(raw, 10, 22)?,
        "fov_start_hundredths_degree_raw": u16_at(raw, 32)?,
        "fov_end_hundredths_degree_raw": u16_at(raw, 34)?,
        "phase_lock_angle_raw": u16_at(raw, 38)?,
        "serial_raw": bytes::<6>(raw, 292)?,
        "zero_angle_offset_raw": u16_at(raw, 298)?,
        "diagnostic_and_misc_base64": encoded(raw, 342, 40)?,
        "gprmc_base64": encoded(raw, 382, 86)?,
        "msop_association": "unavailable",
    });
    let [return_mode] = bytes::<1>(raw, 300)?;
    annotate_mode(&mut value, profile, return_mode);
    match profile {
        Profile::Rs16Difop => {
            value["firmware_info_base64"] = json!(encoded(raw, 40, 10)?);
            value["software_compatibility_raw"] = json!(u16_at(raw, 301)?);
            value["clock"] = rs16_clock(raw, 303)?;
            let mut angles = Vec::with_capacity(16);
            for index in 0..16 {
                angles.push(u24_at(raw, 1165 + index * 3)?);
            }
            value["vertical_calibration_raw"] = json!(angles);
            value["vertical_calibration_base64"] = json!(encoded(raw, 1165, 48)?);
        }
        Profile::Helios16pDifop => {
            value["firmware_info_base64"] = json!(encoded(raw, 40, 43)?);
            let [sync_mode, sync_status] = bytes::<2>(raw, 301)?;
            value["time_sync_mode_raw"] = json!(sync_mode);
            value["time_sync_status_raw"] = json!(sync_status);
            // DIFOP does not carry either MSOP identity variant. Do not infer
            // a newer clock contract from a separately received packet.
            value["clock"] = helios_clock(raw, 303, false)?;
            value["vertical_calibration_raw"] = helios_angles(raw, 468)?;
            value["horizontal_calibration_raw"] = helios_angles(raw, 564)?;
            value["vertical_calibration_base64"] = json!(encoded(raw, 468, 48)?);
            value["horizontal_calibration_base64"] = json!(encoded(raw, 564, 48)?);
        }
        Profile::Helios16pMsop | Profile::Rs16Msop => {
            return Err(unsupported("MSOP profile cannot decode DIFOP data"));
        }
    }
    Ok(value)
}
fn helios_angles(raw: &[u8], offset: usize) -> Result<Value> {
    let mut angles = Vec::with_capacity(16);
    for index in 0..16 {
        let [sign, high, low] = bytes::<3>(raw, offset + index * 3)?;
        angles.push(json!({"sign_raw":sign,"magnitude_raw":u16::from_be_bytes([high, low])}));
    }
    Ok(json!(angles))
}
fn annotate_mode(value: &mut Value, profile: Profile, mode: u8) {
    value["return_mode_raw"] = json!(mode);
    let label = match (profile, mode) {
        (Profile::Rs16Difop | Profile::Helios16pDifop, 0) => Some("dual"),
        (Profile::Rs16Difop, 1) | (Profile::Helios16pDifop, 4) => Some("strongest"),
        (Profile::Rs16Difop, 2) | (Profile::Helios16pDifop, 5) => Some("last"),
        _ => None,
    };
    if let Some(label) = label {
        value["return_mode"] = json!(label);
        value["return_mode_status"] = json!("documented");
    } else if profile == Profile::Helios16pDifop && mode == 6 {
        value["return_mode_status"] = json!("unresolved_vendor_documentation");
        value["return_mode_manual"] = json!("first");
        value["return_mode_sdk"] = json!("nearest");
    } else {
        value["return_mode_status"] = json!("unknown");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HELIOS_MSOP: Format = Format {
        profile: Profile::Helios16pMsop,
        rs16_distance_unit_um: None,
    };
    const HELIOS_DIFOP: Format = Format {
        profile: Profile::Helios16pDifop,
        rs16_distance_unit_um: None,
    };
    const RS16_MSOP: Format = Format {
        profile: Profile::Rs16Msop,
        rs16_distance_unit_um: None,
    };
    const RS16_DIFOP: Format = Format {
        profile: Profile::Rs16Difop,
        rs16_distance_unit_um: None,
    };
    fn put(raw: &mut [u8], offset: usize, value: &[u8]) {
        let Some(target) = raw.get_mut(offset..offset + value.len()) else {
            panic!("literal field is outside fixture");
        };
        target.copy_from_slice(value);
    }
    fn body(raw: &mut Vec<u8>) {
        for azimuth in [
            [0x88, 0xdb],
            [0, 0],
            [0x8c, 0x9f],
            [0x8c, 0xa0],
            [0xff, 0xff],
            [0, 1],
            [0, 2],
            [0, 3],
            [0, 4],
            [0, 5],
            [0, 6],
            [0, 7],
        ] {
            raw.extend_from_slice(&[0xff, 0xee]);
            raw.extend_from_slice(&azimuth);
            for _ in 0..8 {
                raw.extend_from_slice(&[0, 0, 0, 0, 1, 0x7f, 0xff, 0xff, 0xff, 1, 0x40, 1]);
            }
        }
        raw.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef, 0, 0xff]);
        assert_eq!(raw.len(), 1248);
    }
    fn helios_msop() -> Vec<u8> {
        // Manual-derived synthetic, not a device capture. The 88DB azimuth and
        // 0140 distance appear in the published example; its printed8m result
        // contradicts its own0.25cm unit (the exact result is800000um).
        let mut raw = vec![
            0x55, 0xaa, 5, 0x5a, 0, 1, 0xf0, 0xf1, 0, 0, 0, 3, 0xff, 0xff, 0xff, 0xfe, 0x81, 1, 0,
            0x37, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0, 0x0f, 0x42, 0x3f, 0xa0, 6, 3, 0xb0, 0xb1,
            0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8,
        ];
        assert_eq!(raw.len(), 42);
        body(&mut raw);
        raw
    }
    fn rs16_msop() -> Vec<u8> {
        let mut raw = vec![0x55, 0xaa, 5, 0x0a, 0x5a, 0xa5, 0x50, 0xa0];
        raw.extend_from_slice(&[0xa5; 12]);
        // Manufacturer's documented2017-03-10 09:45:30 /100ms/200us bytes.
        raw.extend_from_slice(&[17, 3, 10, 9, 45, 30, 0, 100, 0, 200]);
        raw.extend_from_slice(&[1, 6, 3, 0xff, 0x80, 0, 0xab, 0xcd, 0xef, 1, 2, 3]);
        assert_eq!(raw.len(), 42);
        body(&mut raw);
        // RS16 published example: azimuth0044=>68, distance0642=>1602.
        put(&mut raw, 44, &[0, 0x44]);
        put(&mut raw, 46, &[6, 0x42]);
        raw
    }
    fn difop_literal() -> Vec<u8> {
        let mut raw = (0_u8..=255).cycle().take(1248).collect::<Vec<_>>();
        put(&mut raw, 0, &[0xa5, 0xff, 0, 0x5a, 0x11, 0x11, 0x55, 0x55]);
        put(&mut raw, 8, &[2, 0x58]);
        put(&mut raw, 32, &[0, 0x44, 0x88, 0xdb]);
        put(&mut raw, 38, &[1, 0x0e]);
        put(&mut raw, 292, &[0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc]);
        put(&mut raw, 298, &[0xff, 0xff, 6, 0x80, 0xff]);
        put(&mut raw, 303, &[0xff; 10]);
        put(&mut raw, 382, &[0x80; 86]);
        for index in 0_u16..16 {
            let [high, low] = (index * 257).to_be_bytes();
            let extent = usize::from(index) * 3;
            put(&mut raw, 468 + extent, &[0xff, high, low]);
            put(&mut raw, 564 + extent, &[2, low, high]);
            put(&mut raw, 1165 + extent, &[0x12, 0x34, 0x56]);
        }
        put(&mut raw, 1246, &[0x0f, 0xf0]);
        raw
    }

    #[test]
    fn modern_helios_literal_retains_all_channels_be_boundaries_and_qualified_units() {
        let raw = helios_msop();
        assert_eq!(raw.len(), 1248);
        let decoded = decode(&raw, HELIOS_MSOP).unwrap();
        assert_eq!(decoded.elements, 384);
        let value = decoded.value;
        assert_eq!(value["header_variant"], "type_06_model_03");
        assert_eq!(value["identity_bytes_raw"], json!([0xa0, 6, 3]));
        assert_eq!(value["protocol_version_raw"], 1);
        assert_eq!(value["lidar_type_raw"], 6);
        assert_eq!(value["model_raw"], 3);
        assert_eq!(value["top_board_packet_count_raw"], 3);
        assert_eq!(value["bottom_board_packet_count_raw"], 0xffff_fffe_u32);
        assert_eq!(value["unit_status"], "documented_header");
        assert_eq!(value["distance_unit_um"], 2_500);
        let blocks = value["blocks"].as_array().unwrap();
        assert_eq!(blocks.len(), 12);
        for block in blocks {
            for (key, expected) in [
                ("distance_raw", [0, 1, 65_535, 320]),
                ("distance_um", [0, 2_500, 163_837_500, 800_000]),
                ("reflectivity_raw", [0, 127, 255, 1]),
            ] {
                let array = block[key].as_array().unwrap();
                assert_eq!(array.len(), 32);
                for (actual, expected) in array.iter().zip(expected.into_iter().cycle()) {
                    assert_eq!(actual, &json!(expected));
                }
            }
            assert!(block.get("return_role").is_none());
        }
        assert_eq!(blocks[0]["azimuth_raw"], 35_035);
        assert_eq!(blocks[3]["azimuth_range_status"], "unknown");
        assert_eq!(blocks[4]["azimuth_raw"], 65_535);
        assert_eq!(value["clock"]["seconds_raw"], 281_474_976_710_655_u64);
        assert_eq!(value["clock"]["subsecond_raw"], 999_999);
        assert_eq!(value["clock"]["subsecond_status"], "valid");
        assert_eq!(
            value["clock"]["subsecond_units_status"],
            "documented_microseconds"
        );
        assert_eq!(value["clock"]["synchronization_status"], "unknown");
        assert_eq!(
            STANDARD
                .decode(value["header_base64"].as_str().unwrap())
                .unwrap(),
            raw[..42]
        );
        assert_eq!(
            STANDARD
                .decode(value["tail_reserved_base64"].as_str().unwrap())
                .unwrap(),
            [0xde, 0xad, 0xbe, 0xef]
        );
        for key in [
            "source_time",
            "return_mode",
            "sequence_raw",
            "xyz",
            "frame_id",
        ] {
            assert!(value.get(key).is_none());
        }
        assert!(serde_json::to_vec(&value).unwrap().len() <= decoded_json_bound(HELIOS_MSOP));
    }

    #[test]
    fn helios_legacy_clock_and_unknown_resolution_never_borrow_an_inferred_unit() {
        for (flag, unit) in [(0, Some(5_000)), (1, Some(2_500)), (255, None)] {
            let mut raw = helios_msop();
            put(&mut raw, 17, &[flag]);
            put(&mut raw, 31, &[7, 0xff]);
            put(&mut raw, 26, &[0xff; 4]);
            let value = decode(&raw, HELIOS_MSOP).unwrap().value;
            assert_eq!(value["header_variant"], "legacy_shared_07");
            assert_eq!(value["header_identity_status"], "legacy_shared_family");
            assert_eq!(value["model_raw"], 7);
            assert!(value.get("lidar_type_raw").is_none());
            assert_eq!(value["resolution_flag_raw"], flag);
            assert_eq!(value["clock"]["subsecond_raw"], u32::MAX);
            assert_eq!(
                value["clock"]["subsecond_units_status"],
                "unresolved_vendor_documentation"
            );
            assert!(value["clock"].get("subsecond_unit").is_none());
            if let Some(unit) = unit {
                assert_eq!(value["distance_unit_um"], unit);
                assert_eq!(value["blocks"][0]["distance_um"][2], 65_535 * unit);
            } else {
                assert_eq!(value["unit_status"], "unknown");
                assert!(value.get("distance_unit_um").is_none());
                assert!(value["blocks"][0].get("distance_um").is_none());
            }
        }
        let mut raw = helios_msop();
        put(&mut raw, 26, &[0xff; 4]);
        let value = decode(&raw, HELIOS_MSOP).unwrap().value;
        assert_eq!(value["clock"]["subsecond_raw"], u32::MAX);
        assert_eq!(value["clock"]["subsecond_status"], "invalid_value");
        assert_eq!(value["blocks"].as_array().unwrap().len(), 12);
    }

    #[test]
    fn rs16_default_is_raw_only_and_configured_scales_preserve_the_published_word() {
        let raw = rs16_msop();
        for unit in [None, Some(5000_u16), Some(10_000)] {
            let value = decode(
                &raw,
                Format {
                    rs16_distance_unit_um: unit,
                    ..RS16_MSOP
                },
            )
            .unwrap()
            .value;
            assert_eq!(value["model_raw"], 1);
            assert_eq!(value["blocks"][0]["azimuth_raw"], 68);
            assert_eq!(value["blocks"][0]["distance_raw"][0], 1602);
            assert_eq!(
                value["clock"]["calendar_fields_raw"],
                json!([17, 3, 10, 9, 45, 30])
            );
            assert_eq!(value["clock"]["millisecond_word_raw"], 100);
            assert_eq!(value["clock"]["microsecond_word_raw"], 200);
            assert_eq!(value["clock"]["calendar_decoding"], "decoded_unzoned");
            assert_eq!(
                value["clock"]["calendar_unzoned"],
                "2017-03-10T09:45:30.100200"
            );
            if let Some(unit) = unit {
                assert_eq!(value["unit_status"], "configured_unverified");
                assert_eq!(value["unit_origin"], "configured");
                assert_eq!(value["distance_unit_um"], unit);
                assert_eq!(
                    value["blocks"][0]["distance_um"][0],
                    1602_u32 * u32::from(unit)
                );
                assert_eq!(
                    value["blocks"][0]["distance_um"][2],
                    65_535_u32 * u32::from(unit)
                );
            } else {
                assert_eq!(value["unit_status"], "unavailable_firmware_dependent");
                assert!(value.get("distance_unit_um").is_none());
                assert!(value["blocks"][0].get("distance_um").is_none());
            }
            assert!(value.get("resolution_flag_raw").is_none());
            assert!(serde_json::to_vec(&value).unwrap().len() <= decoded_json_bound(RS16_MSOP));
        }
    }

    #[test]
    fn fixed_msop_markers_and_explicit_identity_offsets_are_strict_without_neighbor_fallback() {
        for (offset, replacement, kind, diagnostic) in [
            (0, vec![0], ErrorKind::Incomplete, "header marker"),
            (4, vec![0, 2], ErrorKind::Unsupported, "version"),
            (30, vec![6, 0, 3], ErrorKind::Unsupported, "type/model"),
            (31, vec![6, 2], ErrorKind::Unsupported, "type/model"),
            (42, vec![0xee, 0xff], ErrorKind::Incomplete, "block marker"),
            (1142, vec![0, 0], ErrorKind::Incomplete, "block marker"),
            (1246, vec![0xff, 0], ErrorKind::Incomplete, "tail marker"),
        ] {
            let mut raw = helios_msop();
            put(&mut raw, offset, &replacement);
            let error = decode(&raw, HELIOS_MSOP).unwrap_err();
            assert_eq!(crate::types::classify(&error).kind, kind);
            assert!(error.to_string().contains(diagnostic));
        }
        let mut wrong = rs16_msop();
        put(&mut wrong, 30, &[0, 1]);
        let error = decode(&wrong, RS16_MSOP).unwrap_err();
        assert_eq!(crate::types::classify(&error).kind, ErrorKind::Unsupported);
        assert!(error.to_string().contains("model"));
        decode(&rs16_msop(), HELIOS_MSOP).unwrap_err();
        decode(&helios_msop(), RS16_MSOP).unwrap_err();
    }

    #[test]
    fn difop_profiles_keep_the_full_data_region_status_calibration_and_gprmc_bytes() {
        let raw = difop_literal();
        assert_eq!(raw.len(), 1248);
        for format in [HELIOS_DIFOP, RS16_DIFOP] {
            let decoded = decode(&raw, format).unwrap();
            assert_eq!(decoded.elements, 1);
            let value = decoded.value;
            assert_eq!(value["packet_type"], "difop");
            assert_eq!(value["motor_speed_rpm"], 600);
            assert_eq!(
                value["serial_raw"],
                json!([0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc])
            );
            assert_eq!(value["zero_angle_offset_raw"], 65_535);
            assert_eq!(value["phase_lock_angle_raw"], 270);
            assert_eq!(value["fov_start_hundredths_degree_raw"], 68);
            assert_eq!(value["fov_end_hundredths_degree_raw"], 35_035);
            assert_eq!(value["msop_association"], "unavailable");
            for (key, offset, length) in [
                ("data_base64", 8, 1238),
                ("network_base64", 10, 22),
                ("diagnostic_and_misc_base64", 342, 40),
                ("gprmc_base64", 382, 86),
            ] {
                assert_eq!(
                    STANDARD.decode(value[key].as_str().unwrap()).unwrap(),
                    span(&raw, offset, length).unwrap()
                );
            }
            if format.profile == Profile::Rs16Difop {
                assert_eq!(value["software_compatibility_raw"], 0x80ff);
                assert_eq!(value["clock"]["millisecond_word_raw"], 65_535);
                assert_eq!(value["clock"]["microsecond_word_raw"], 65_535);
                assert_eq!(
                    value["vertical_calibration_raw"],
                    json!(vec![1_193_046; 16])
                );
                assert_eq!(
                    STANDARD
                        .decode(value["vertical_calibration_base64"].as_str().unwrap())
                        .unwrap(),
                    raw[1165..1213]
                );
                assert!(value.get("horizontal_calibration_raw").is_none());
            } else {
                assert_eq!(value["time_sync_mode_raw"], 128);
                assert_eq!(value["time_sync_status_raw"], 255);
                assert_eq!(value["clock"]["seconds_raw"], 281_474_976_710_655_u64);
                assert_eq!(value["clock"]["subsecond_raw"], u32::MAX);
                assert_eq!(
                    value["clock"]["subsecond_units_status"],
                    "unresolved_vendor_documentation"
                );
                for key in ["vertical_calibration_raw", "horizontal_calibration_raw"] {
                    assert_eq!(value[key].as_array().unwrap().len(), 16);
                }
                assert_eq!(
                    value["vertical_calibration_raw"][15],
                    json!({"sign_raw":255,"magnitude_raw":3_855})
                );
                assert_eq!(
                    value["horizontal_calibration_raw"][15],
                    json!({"sign_raw":2,"magnitude_raw":3_855})
                );
            }
            assert!(value.get("source_time").is_none());
            assert!(value.get("sequence_raw").is_none());
            assert!(serde_json::to_vec(&value).unwrap().len() <= decoded_json_bound(format));
        }
    }

    #[test]
    fn difop_return_codes_unknown_states_and_mode06_wording_never_become_msop_roles() {
        for (format, codes) in [
            (
                RS16_DIFOP,
                [(0, Some("dual")), (1, Some("strongest")), (2, Some("last"))],
            ),
            (
                HELIOS_DIFOP,
                [(0, Some("dual")), (4, Some("strongest")), (5, Some("last"))],
            ),
        ] {
            for (code, label) in codes {
                let mut raw = difop_literal();
                put(&mut raw, 300, &[code]);
                let value = decode(&raw, format).unwrap().value;
                assert_eq!(value["return_mode_raw"], code);
                assert_eq!(value["return_mode_status"], "documented");
                assert_eq!(value["return_mode"].as_str(), label);
            }
            for code in [6, 255] {
                let mut raw = difop_literal();
                put(&mut raw, 300, &[code]);
                let value = decode(&raw, format).unwrap().value;
                assert_eq!(value["return_mode_raw"], code);
                assert!(value.get("return_mode").is_none());
                if code == 6 && format.profile == Profile::Helios16pDifop {
                    assert_eq!(
                        value["return_mode_status"],
                        "unresolved_vendor_documentation"
                    );
                    assert_eq!(value["return_mode_manual"], "first");
                    assert_eq!(value["return_mode_sdk"], "nearest");
                } else {
                    assert_eq!(value["return_mode_status"], "unknown");
                }
                assert_eq!(value["msop_association"], "unavailable");
            }
        }
    }

    #[test]
    fn optional_rs16_unit_is_bounded_and_rejected_for_every_other_profile() {
        for unit in [0, 1, 2_500, 4_999, 5_001, 9_999, 10_001, u16::MAX] {
            let error = decode(
                &rs16_msop(),
                Format {
                    rs16_distance_unit_um: Some(unit),
                    ..RS16_MSOP
                },
            )
            .unwrap_err();
            assert_eq!(crate::types::classify(&error).kind, ErrorKind::Usage);
        }
        for format in [HELIOS_MSOP, HELIOS_DIFOP, RS16_DIFOP] {
            let error = decode(
                &difop_literal(),
                Format {
                    rs16_distance_unit_um: Some(5_000),
                    ..format
                },
            )
            .unwrap_err();
            assert_eq!(crate::types::classify(&error).kind, ErrorKind::Usage);
        }
    }

    #[test]
    fn all_profile_extents_and_difop_envelopes_are_checked_before_projection() {
        for (format, raw) in [
            (HELIOS_MSOP, helios_msop()),
            (RS16_MSOP, rs16_msop()),
            (HELIOS_DIFOP, difop_literal()),
            (RS16_DIFOP, difop_literal()),
        ] {
            assert_eq!(expected_packet_bytes(format), 1248);
            for length in 0..1248 {
                let Some(prefix) = raw.get(..length) else {
                    panic!("fixture prefix extent");
                };
                let error = decode(prefix, format).unwrap_err();
                assert_eq!(crate::types::classify(&error).kind, ErrorKind::Incomplete);
            }
            let mut extra = raw;
            extra.push(0);
            decode(&extra, format).unwrap_err();
        }
        for format in [HELIOS_DIFOP, RS16_DIFOP] {
            for offset in [0, 7, 1246, 1247] {
                let mut raw = difop_literal();
                put(&mut raw, offset, &[0]);
                let error = decode(&raw, format).unwrap_err();
                assert_eq!(crate::types::classify(&error).kind, ErrorKind::Incomplete);
            }
            decode(&helios_msop(), format).unwrap_err();
            decode(&rs16_msop(), format).unwrap_err();
        }
    }

    #[test]
    fn rs16_calendar_requires_zero_reserved_bits_and_valid_nominal_fields() {
        for (clock, status, text) in [
            (
                [0, 2, 29, 0, 0, 0, 3, 0xe7, 3, 0xe7],
                "valid",
                "2000-02-29T00:00:00.999999",
            ),
            ([100, 2, 29, 0, 0, 0, 0, 0, 0, 0], "invalid_value", ""),
            (
                [255, 12, 31, 23, 59, 59, 0, 0, 0, 1],
                "valid",
                "2255-12-31T23:59:59.000001",
            ),
            (
                [0, 0x82, 29, 0, 0, 0, 0, 0, 0, 0],
                "unknown_reserved_bits",
                "",
            ),
            (
                [0, 2, 29, 0, 0, 0, 0x80, 0, 0, 0],
                "unknown_reserved_bits",
                "",
            ),
            ([0, 2, 29, 0, 0, 0, 3, 0xe8, 0, 0], "invalid_value", ""),
            ([0, 2, 29, 0, 0, 0, 0, 0, 3, 0xe8], "invalid_value", ""),
        ] {
            for (mut raw, format, offset) in [
                (rs16_msop(), RS16_MSOP, 20),
                (difop_literal(), RS16_DIFOP, 303),
            ] {
                put(&mut raw, offset, &clock);
                let value = decode(&raw, format).unwrap().value;
                let native = &value["clock"];
                assert_eq!(
                    STANDARD
                        .decode(native["raw_base64"].as_str().unwrap())
                        .unwrap(),
                    clock
                );
                assert_eq!(native["calendar_status"], status);
                assert_eq!(native["synchronization_status"], "unknown");
                if text.is_empty() {
                    assert_eq!(native["calendar_decoding"], "raw_only");
                    assert!(native.get("calendar_unzoned").is_none());
                } else {
                    assert_eq!(native["calendar_decoding"], "decoded_unzoned");
                    assert_eq!(native["reserved_bits_status"], "zero");
                    assert_eq!(native["calendar_unzoned"], text);
                }
                if status == "unknown_reserved_bits" {
                    assert_eq!(native["reserved_bits_status"], "nonzero");
                }
            }
        }
        let value = decode(&difop_literal(), RS16_DIFOP).unwrap().value;
        assert_eq!(
            value["clock"]["reserved_bits_raw"],
            json!({
                "month":240,"day":224,"hour":224,"minute":192,"second":192,
                "millisecond":64_512,"microsecond":64_512,
            })
        );
    }
}
