//! Native fields from the explicit `PandarXT` 32-channel and GPS wire profiles.
//!
//! Manufacturer manual X01-en-250220, sections 3.1/3.2 and Appendix B:
//! <https://www.hesaitech.com/wp-content/uploads/2025/02/PandarXT_User_Manual_X01-en-250220.pdf>
//! Layout independently checked against official SDK `udp_protocol_v6_1.h` at
//! `fbcd94a1efb7af0133f9512e14afdde85e5bc06b`; no SDK source is copied.
use crate::types::{Categorized, ErrorKind};
use anyhow::Result;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::io::{self, Write};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Profile {
    PandarXt32V6_1,
    PandarXt32Gps,
}
impl Profile {
    pub(super) const fn name(self) -> &'static str {
        match self {
            Self::PandarXt32V6_1 => "pandar_xt32_v6_1",
            Self::PandarXt32Gps => "pandar_xt32_gps",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) struct Format {
    pub profile: Profile,
}

pub(super) const fn expected_packet_bytes(format: Format) -> usize {
    match format.profile {
        Profile::PandarXt32V6_1 => 1080,
        Profile::PandarXt32Gps => 512,
    }
}

/// Conservative serialized bound for the closed, fixed-size native schemas.
pub(super) const fn decoded_json_bound(format: Format) -> usize {
    // Lidar has 8*32 values in four dense arrays (<=7B per integer including
    // separators), eight small block objects and <4KiB of fixed metadata.
    // GPS has 496 raw bytes in base64 plus two short ASCII-byte arrays.
    match format.profile {
        Profile::PandarXt32V6_1 => 128 * 1024,
        Profile::PandarXt32Gps => 4096,
    }
}

#[derive(Debug)]
pub(super) struct Decoded {
    pub value: Value,
    pub elements: usize,
}

fn malformed(message: &str) -> anyhow::Error {
    Categorized::error(ErrorKind::Incomplete, format!("Hesai: {message}"))
}
fn unsupported(message: &str) -> anyhow::Error {
    Categorized::error(ErrorKind::Unsupported, format!("Hesai: {message}"))
}
fn bytes<const N: usize>(raw: &[u8], offset: usize) -> Result<[u8; N]> {
    let end = offset
        .checked_add(N)
        .ok_or_else(|| malformed("field offset overflow"))?;
    raw.get(offset..end)
        .ok_or_else(|| malformed("truncated field"))?
        .try_into()
        .map_err(|_| malformed("incorrect field extent"))
}
fn u16_at(raw: &[u8], offset: usize) -> Result<u16> {
    Ok(u16::from_le_bytes(bytes(raw, offset)?))
}
fn u32_at(raw: &[u8], offset: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(bytes(raw, offset)?))
}

struct JsonSize {
    bytes: usize,
    limit: usize,
}
impl Write for JsonSize {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.bytes = self
            .bytes
            .checked_add(buf.len())
            .filter(|n| *n <= self.limit)
            .ok_or_else(|| io::Error::other("Hesai decoded JSON exceeds its bounded schema"))?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) fn decode(raw: &[u8], format: Format) -> Result<Decoded> {
    if raw.len() != expected_packet_bytes(format) {
        return Err(malformed(
            "datagram length does not match the selected profile",
        ));
    }
    let (value, elements) = match format.profile {
        Profile::PandarXt32V6_1 => (lidar(raw, format.profile)?, 256),
        Profile::PandarXt32Gps => (gps(raw, format.profile)?, 1),
    };
    serde_json::to_writer(
        JsonSize {
            bytes: 0,
            limit: decoded_json_bound(format),
        },
        &value,
    )
    .map_err(|error| {
        Categorized::error(
            ErrorKind::OperationFailed,
            format!("Hesai: bounded native serialization failed: {error}"),
        )
    })?;
    Ok(Decoded { value, elements })
}

fn mode(raw: u8) -> Result<(&'static str, u8)> {
    match raw {
        0x33 => Ok(("first", 1)),
        0x37 => Ok(("strongest", 1)),
        0x38 => Ok(("last", 1)),
        0x39 => Ok(("last_and_strongest", 2)),
        0x3b => Ok(("last_and_first", 2)),
        0x3c => Ok(("first_and_strongest", 2)),
        _ => Err(unsupported("return mode is outside the documented profile")),
    }
}

fn lidar(raw: &[u8], profile: Profile) -> Result<Value> {
    let [
        start0,
        start1,
        major,
        minor,
        reserved0,
        reserved1,
        channels,
        blocks,
        first,
        unit,
        returns,
        flags,
    ] = bytes::<12>(raw, 0)?;
    if [start0, start1] != [0xee, 0xff] {
        return Err(malformed("point-cloud start marker is not EEFF"));
    }
    if [major, minor] != [6, 1] {
        return Err(unsupported("point-cloud protocol version is not 6.1"));
    }
    if channels != 32 || blocks != 8 || unit != 4 || returns != 2 {
        return Err(malformed(
            "fixed channel/block/unit/maximum-return header contradicts the selected profile",
        ));
    }
    // Only bit0 has defined semantics in the selected manual. The remaining
    // status bits and First Block Return byte are reserved, not echo labels.
    if flags & 1 == 0 {
        return Err(malformed("required UDP sequence flag is absent"));
    }
    let [return_mode_raw] = bytes::<1>(raw, 1062)?;
    let (return_mode, returns_per_firing) = mode(return_mode_raw)?;
    let [factory] = bytes::<1>(raw, 1075)?;
    if factory != 0x42 {
        return Err(malformed(
            "factory information contradicts the selected profile",
        ));
    }
    if returns_per_firing == 2 {
        // The manual explicitly requires equal raw azimuths in each adjacent
        // pair. It does not promise that the second return is always strongest.
        for pair in 0..4 {
            let offset = 12 + pair * 260;
            if u16_at(raw, offset)? != u16_at(raw, offset + 130)? {
                return Err(malformed("dual-return pair has unequal azimuths"));
            }
        }
    }
    let mut decoded_blocks = Vec::with_capacity(8);
    for index in 0..8 {
        let start = 12 + index * 130;
        let azimuth = u16_at(raw, start)?;
        let mut distances = Vec::with_capacity(32);
        let mut millimeters = Vec::with_capacity(32);
        let mut reflectivity = Vec::with_capacity(32);
        let mut extra = Vec::with_capacity(32);
        for channel in 0..32 {
            let [low, high, reflection, reserved] = bytes::<4>(raw, start + 2 + channel * 4)?;
            let distance = u16::from_le_bytes([low, high]);
            distances.push(distance);
            millimeters.push(u32::from(distance) * 4);
            reflectivity.push(reflection);
            extra.push(reserved);
        }
        decoded_blocks.push(json!({
            "index": index,
            "azimuth_raw": azimuth,
            "azimuth_hundredths_degree": azimuth,
            // The table gives units but no numeric wire bound. Preserve all
            // u16 values; an unusual reference angle is not a framing failure.
            "azimuth_range_status": if azimuth < 36_000 { "within_nominal_rotation" } else { "unknown" },
            "distance_raw": distances,
            "distance_mm": millimeters,
            "reflectivity_raw": reflectivity,
            "channel_extra_raw": extra,
        }));
    }
    Ok(json!({
        "profile": profile.name(),
        "protocol_version_major_raw": major,
        "protocol_version_minor_raw": minor,
        "preheader_reserved_raw": [reserved0, reserved1],
        "header": {
            "channel_count_raw": channels,
            "block_count_raw": blocks,
            "first_block_return_reserved_raw": first,
            "distance_unit_mm": unit,
            "maximum_returns_raw": returns,
            "sequence_flags_raw": flags,
        },
        "return_mode_raw": return_mode_raw,
        "return_mode": return_mode,
        "returns_per_firing": returns_per_firing,
        "motor_speed_rpm": u16_at(raw, 1063)?,
        "factory_info_raw": factory,
        "sequence_raw": u32_at(raw, 1076)?,
        "tail_reserved_base64": STANDARD.encode(bytes::<10>(raw, 1052)?),
        "checksum_status": "no_checksum_in_format",
        "clock": clock(bytes(raw, 1065)?, u32_at(raw, 1071)?),
        "blocks": decoded_blocks,
    }))
}

fn clock(calendar: [u8; 6], micros: u32) -> Value {
    let [year_byte, month, day, hour, minute, second] = calendar;
    let year = 1900 + u16::from(year_byte);
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => 28 + u8::from(leap),
        _ => 0,
    };
    let valid =
        year_byte >= 70 && day > 0 && day <= days && hour < 24 && minute < 60 && second < 60;
    let mut value = json!({
        "calendar_raw": calendar,
        "calendar_status": if valid { "valid" } else { "invalid_value" },
        "microsecond_raw": micros,
        "microsecond_status": if micros <= 999_999 { "valid" } else { "invalid_value" },
        "synchronization_status": "unknown",
    });
    if valid {
        value["calendar_unzoned"] =
            format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}").into();
    }
    value
}

fn gps(raw: &[u8], profile: Profile) -> Result<Value> {
    let header = bytes::<2>(raw, 0)?;
    if header != [0xff, 0xee] {
        return Err(malformed("GPS start marker is not FFEE"));
    }
    let reserved = bytes::<404>(raw, 102)?;
    let [position] = bytes::<1>(raw, 506)?;
    let [pps] = bytes::<1>(raw, 507)?;
    Ok(json!({
        "profile": profile.name(),
        "gps_header_raw": header,
        "date_ascii_raw": bytes::<6>(raw, 2)?,
        "time_ascii_raw": bytes::<6>(raw, 8)?,
        "time_reserved_base64": STANDARD.encode(bytes::<4>(raw, 14)?),
        "nmea_raw_base64": STANDARD.encode(bytes::<84>(raw, 18)?),
        "reserved_base64": STANDARD.encode(reserved.as_slice()),
        "reserved_pattern_status": if reserved.iter().all(|&byte| byte == 0xdf) { "expected_df" } else { "unexpected" },
        "positioning_status_raw": position,
        // Meaning depends on the selected external NMEA sentence. Raw capture
        // alone must not interpret a digit as RMC A/V, or assert a GPS fix.
        "positioning_status_state": "unknown",
        "pps_lock_raw": pps,
        "pps_lock_status": match pps { 0 => "unlocked", 1 => "locked", _ => "unknown" },
        "tail_reserved_base64": STANDARD.encode(bytes::<4>(raw, 508)?),
        "checksum_status": "no_checksum_in_format",
        "synchronization_status": "unknown",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIDAR: Format = Format {
        profile: Profile::PandarXt32V6_1,
    };
    const GPS: Format = Format {
        profile: Profile::PandarXt32Gps,
    };

    fn put(raw: &mut [u8], offset: usize, value: &[u8]) {
        let end = offset
            .checked_add(value.len())
            .expect("literal offset overflow");
        assert!(end <= raw.len(), "literal exceeds its declared extent");
        raw[offset..end].copy_from_slice(value);
    }

    fn literal_lidar() -> Vec<u8> {
        // Manual-derived synthetic, not a captured device packet. Literal LE
        // boundaries deliberately avoid the production offset/geometry helpers.
        let mut raw = vec![0xee, 0xff, 6, 1, 0x55, 0xaa, 32, 8, 0x96, 4, 2, 0x81];
        for azimuth in [
            [0, 0],
            [0, 0],
            [0x9f, 0x8c],
            [0x9f, 0x8c],
            [0xa0, 0x8c],
            [0xa0, 0x8c],
            [0xff, 0xff],
            [0xff, 0xff],
        ] {
            raw.extend_from_slice(&azimuth);
            for _ in 0..8 {
                raw.extend_from_slice(&[
                    0, 0, 0, 0, 1, 0, 0x7f, 0x80, 0xff, 0xff, 0xff, 0xff, 0x34, 0x12, 1, 0xa5,
                ]);
            }
        }
        raw.extend_from_slice(&[
            0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 0x39, 0x58, 2, 124, 2, 29, 23, 59, 58, 0x3f, 0x42, 0x0f,
            0, 0x42, 0xfe, 0xff, 0xff, 0xff,
        ]);
        assert_eq!(raw.len(), 1080);
        raw
    }

    fn literal_gps() -> Vec<u8> {
        // Date/time bytes from the manual's GPS analysis example: reversed
        // digit pairs retained exactly, without a century/calendar inference.
        let mut raw = vec![0xff, 0xee];
        raw.extend_from_slice(b"024070857040");
        raw.extend_from_slice(&[0x11, 0x22, 0x33, 0x44]);
        let mut nmea = [0x80; 84];
        put(
            &mut nmea,
            0,
            b"$GPRMC,072242,A,3027.3680,N,11423.6975,E,000.0,316.7,160617,004.1,W*67",
        );
        raw.extend_from_slice(&nmea);
        raw.extend_from_slice(&[0xdf; 404]);
        raw.extend_from_slice(&[b'A', 1, 0x55, 0xaa, 0xff, 0]);
        assert_eq!(raw.len(), 512);
        raw
    }

    #[test]
    fn literal_lidar_keeps_dense_boundary_values_reserved_bytes_and_clock() {
        let raw = literal_lidar();
        let decoded = decode(&raw, LIDAR).unwrap();
        let value = decoded.value;
        assert_eq!(decoded.elements, 256);
        assert_eq!(value["profile"], "pandar_xt32_v6_1");
        assert_eq!(value["preheader_reserved_raw"], json!([0x55, 0xaa]));
        assert_eq!(value["header"]["first_block_return_reserved_raw"], 0x96);
        assert_eq!(value["header"]["sequence_flags_raw"], 0x81);
        assert_eq!(value["sequence_raw"], 0xffff_fffe_u32);
        assert_eq!(value["factory_info_raw"], 0x42);
        assert_eq!(value["motor_speed_rpm"], 600);
        assert_eq!(value["clock"]["calendar_unzoned"], "2024-02-29T23:59:58");
        assert_eq!(value["clock"]["microsecond_raw"], 999_999);
        assert_eq!(value["clock"]["microsecond_status"], "valid");
        assert_eq!(value["clock"]["synchronization_status"], "unknown");
        assert_eq!(
            STANDARD
                .decode(value["tail_reserved_base64"].as_str().unwrap())
                .unwrap(),
            [0, 1, 2, 3, 4, 5, 6, 7, 8, 9]
        );
        let blocks = value["blocks"].as_array().unwrap();
        assert_eq!(blocks.len(), 8);
        for block in blocks {
            for (key, expected) in [
                ("distance_raw", [0, 1, 65_535, 4660]),
                ("distance_mm", [0, 4, 262_140, 18_640]),
                ("reflectivity_raw", [0, 127, 255, 1]),
                ("channel_extra_raw", [0, 128, 255, 165]),
            ] {
                let array = block[key].as_array().unwrap();
                assert_eq!(array.len(), 32);
                for (actual, expected) in array.iter().zip(expected.into_iter().cycle()) {
                    assert_eq!(actual, &json!(expected));
                }
            }
            assert!(block.get("return_role").is_none());
        }
        assert_eq!(blocks[2]["azimuth_raw"], 35_999);
        assert_eq!(blocks[2]["azimuth_range_status"], "within_nominal_rotation");
        assert_eq!(blocks[4]["azimuth_raw"], 36_000);
        assert_eq!(blocks[4]["azimuth_range_status"], "unknown");
        assert_eq!(blocks[6]["azimuth_raw"], 65_535);
        assert_eq!(blocks[6]["azimuth_hundredths_degree"], 65_535);
        assert_eq!(blocks[6]["azimuth_range_status"], "unknown");
        assert!(value.get("source_time").is_none());
        assert!(value.get("confidence").is_none());
        assert!(serde_json::to_vec(&value).unwrap().len() <= decoded_json_bound(LIDAR));
    }

    #[test]
    fn six_return_modes_keep_duplicate_returns_and_only_dual_pairs_require_equal_azimuth() {
        for (raw_mode, name, count) in [
            (0x33, "first", 1),
            (0x37, "strongest", 1),
            (0x38, "last", 1),
            (0x39, "last_and_strongest", 2),
            (0x3b, "last_and_first", 2),
            (0x3c, "first_and_strongest", 2),
        ] {
            let mut raw = literal_lidar();
            put(&mut raw, 1062, &[raw_mode]);
            let value = decode(&raw, LIDAR).unwrap().value;
            assert_eq!(value["return_mode_raw"], raw_mode);
            assert_eq!(value["return_mode"], name);
            assert_eq!(value["returns_per_firing"], count);
            assert_eq!(value["blocks"].as_array().unwrap().len(), 8);
            assert_eq!(
                value["blocks"][0]["distance_raw"],
                value["blocks"][1]["distance_raw"]
            );
            for pair in 0..4 {
                let mut wrong = raw.clone();
                put(&mut wrong, 142 + pair * 260, &[1, 0]);
                if count == 2 {
                    let error = decode(&wrong, LIDAR).unwrap_err();
                    assert_eq!(crate::types::classify(&error).kind, ErrorKind::Incomplete);
                    assert!(error.to_string().contains("unequal azimuths"));
                } else {
                    decode(&wrong, LIDAR).unwrap();
                }
            }
        }
    }

    #[test]
    fn malformed_fixed_headers_are_distinct_from_unsupported_versions_and_modes() {
        for (offset, byte, kind, diagnostic) in [
            (0, 0, ErrorKind::Incomplete, "start marker"),
            (1, 0, ErrorKind::Incomplete, "start marker"),
            (2, 7, ErrorKind::Unsupported, "version"),
            (3, 0, ErrorKind::Unsupported, "version"),
            (6, 16, ErrorKind::Incomplete, "fixed channel"),
            (7, 7, ErrorKind::Incomplete, "fixed channel"),
            (9, 2, ErrorKind::Incomplete, "fixed channel"),
            (10, 1, ErrorKind::Incomplete, "fixed channel"),
            (11, 0x80, ErrorKind::Incomplete, "sequence flag"),
            (1062, 0x3a, ErrorKind::Unsupported, "return mode"),
            (1075, 0, ErrorKind::Incomplete, "factory"),
        ] {
            let mut raw = literal_lidar();
            put(&mut raw, offset, &[byte]);
            let error = decode(&raw, LIDAR).unwrap_err();
            assert_eq!(crate::types::classify(&error).kind, kind);
            assert!(error.to_string().contains(diagnostic));
        }
        let raw = literal_lidar();
        for length in 0..1080 {
            let error = decode(&raw[..length], LIDAR).unwrap_err();
            assert_eq!(crate::types::classify(&error).kind, ErrorKind::Incomplete);
        }
        let mut extra = raw;
        extra.push(0);
        decode(&extra, LIDAR).unwrap_err();
    }

    #[test]
    fn raw_calendar_and_microseconds_remain_observations_when_application_values_are_invalid() {
        for (calendar, valid, text) in [
            ([70, 1, 1, 0, 0, 0], true, "1970-01-01T00:00:00"),
            ([100, 2, 29, 12, 30, 59], true, "2000-02-29T12:30:59"),
            ([200, 2, 29, 0, 0, 0], false, ""),
            ([255, 12, 31, 23, 59, 59], true, "2155-12-31T23:59:59"),
            ([69, 12, 31, 0, 0, 0], false, ""),
            ([124, 0, 1, 0, 0, 0], false, ""),
            ([124, 13, 1, 0, 0, 0], false, ""),
            ([124, 4, 31, 0, 0, 0], false, ""),
            ([124, 2, 0, 0, 0, 0], false, ""),
            ([124, 2, 1, 24, 0, 0], false, ""),
            ([124, 2, 1, 0, 60, 0], false, ""),
            ([124, 2, 1, 0, 0, 60], false, ""),
        ] {
            let mut raw = literal_lidar();
            put(&mut raw, 1065, &calendar);
            let value = decode(&raw, LIDAR).unwrap().value;
            assert_eq!(value["clock"]["calendar_raw"], json!(calendar));
            if valid {
                assert_eq!(value["clock"]["calendar_status"], "valid");
                assert_eq!(value["clock"]["calendar_unzoned"], text);
            } else {
                assert_eq!(value["clock"]["calendar_status"], "invalid_value");
                assert!(value["clock"].get("calendar_unzoned").is_none());
            }
            assert_eq!(value["blocks"].as_array().unwrap().len(), 8);
        }
        for (micros, status) in [
            (0_u32, "valid"),
            (999_999, "valid"),
            (1_000_000, "invalid_value"),
            (u32::MAX, "invalid_value"),
        ] {
            let mut raw = literal_lidar();
            put(&mut raw, 1071, &micros.to_le_bytes());
            let value = decode(&raw, LIDAR).unwrap().value;
            assert_eq!(value["clock"]["microsecond_raw"], micros);
            assert_eq!(value["clock"]["microsecond_status"], status);
            assert_eq!(value["clock"]["synchronization_status"], "unknown");
        }
    }

    #[test]
    fn reserved_flags_and_full_u32_sequence_are_exact_without_loss_inference() {
        for flags in [1, 0x81, 0xff] {
            for sequence in [0_u32, 1, u32::MAX] {
                let mut raw = literal_lidar();
                put(&mut raw, 11, &[flags]);
                put(&mut raw, 1076, &sequence.to_le_bytes());
                let value = decode(&raw, LIDAR).unwrap().value;
                assert_eq!(value["header"]["sequence_flags_raw"], flags);
                assert_eq!(value["sequence_raw"], sequence);
                assert!(value.get("sequence_observation").is_none());
                assert!(value.get("lost_packets").is_none());
            }
        }
    }

    #[test]
    fn gps_keeps_the_manual_ascii_order_all_nmea_bytes_and_reserved_regions() {
        let raw = literal_gps();
        let decoded = decode(&raw, GPS).unwrap();
        assert_eq!(decoded.elements, 1);
        let value = decoded.value;
        assert_eq!(value["profile"], "pandar_xt32_gps");
        assert_eq!(value["gps_header_raw"], json!([255, 238]));
        assert_eq!(value["date_ascii_raw"], json!([48, 50, 52, 48, 55, 48]));
        assert_eq!(value["time_ascii_raw"], json!([56, 53, 55, 48, 52, 48]));
        for (key, expected) in [
            ("time_reserved_base64", &raw[14..18]),
            ("nmea_raw_base64", &raw[18..102]),
            ("reserved_base64", &raw[102..506]),
            ("tail_reserved_base64", &raw[508..512]),
        ] {
            assert_eq!(
                STANDARD.decode(value[key].as_str().unwrap()).unwrap(),
                expected
            );
        }
        assert_eq!(value["reserved_pattern_status"], "expected_df");
        assert_eq!(value["positioning_status_raw"], 65);
        assert_eq!(value["positioning_status_state"], "unknown");
        assert_eq!(value["pps_lock_raw"], 1);
        assert_eq!(value["pps_lock_status"], "locked");
        assert_eq!(value["synchronization_status"], "unknown");
        for key in [
            "sequence_raw",
            "source_time",
            "calendar_unzoned",
            "latitude",
            "blocks",
        ] {
            assert!(value.get(key).is_none());
        }
        assert!(serde_json::to_vec(&value).unwrap().len() <= decoded_json_bound(GPS));
    }

    #[test]
    fn gps_unknown_application_status_and_arbitrary_nmea_ascii_bytes_are_retained() {
        for (pps, name) in [(0, "unlocked"), (1, "locked"), (255, "unknown")] {
            let mut raw = literal_gps();
            put(&mut raw, 2, &[0xff, 0, 0x80, b'X', b'Y', b'Z']);
            put(&mut raw, 18, &[0xff; 84]);
            put(&mut raw, 102, &[0]);
            put(&mut raw, 506, &[0x80, pps]);
            let value = decode(&raw, GPS).unwrap().value;
            assert_eq!(value["date_ascii_raw"], json!([255, 0, 128, 88, 89, 90]));
            assert_eq!(
                STANDARD
                    .decode(value["nmea_raw_base64"].as_str().unwrap())
                    .unwrap(),
                [0xff; 84]
            );
            assert_eq!(value["reserved_pattern_status"], "unexpected");
            assert_eq!(value["positioning_status_raw"], 128);
            assert_eq!(value["positioning_status_state"], "unknown");
            assert_eq!(value["pps_lock_raw"], pps);
            assert_eq!(value["pps_lock_status"], name);
        }
    }

    #[test]
    fn gps_and_lidar_profiles_never_guess_a_different_length_or_marker() {
        let lidar = literal_lidar();
        let gps = literal_gps();
        decode(&lidar, GPS).unwrap_err();
        decode(&gps, LIDAR).unwrap_err();
        for length in 0..512 {
            decode(&gps[..length], GPS).unwrap_err();
        }
        let mut extra = gps.clone();
        extra.push(0);
        decode(&extra, GPS).unwrap_err();
        for marker in [[0xee, 0xff], [0, 0]] {
            let mut wrong = gps.clone();
            put(&mut wrong, 0, &marker);
            let error = decode(&wrong, GPS).unwrap_err();
            assert_eq!(crate::types::classify(&error).kind, ErrorKind::Incomplete);
            assert!(error.to_string().contains("GPS start marker"));
        }
        assert_eq!(expected_packet_bytes(LIDAR), 1080);
        assert_eq!(expected_packet_bytes(GPS), 512);
    }
}
