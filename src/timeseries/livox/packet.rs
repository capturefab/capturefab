//! Qualified native Livox samples, without clock, pose or scan reconstruction.
//!
//! SDK2 wire v0: Mid-360/S/L and HAP TX/T1; legacy wire v5: Horizon/Avia.
//! Source qualification and independent byte recipes are pinned in the Livox plan.
use crate::types::{Categorized, ErrorKind};
use anyhow::Result;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::{self, Write};

const MAX_JSON: usize = 128 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Profile {
    Sdk2Mid360V0,
    Sdk2Mid360sV0,
    Sdk2Mid360lV0,
    Sdk2HapTxV0,
    Sdk2HapT1V0,
    LegacyHorizonV5,
    LegacyAviaV5,
}
impl Profile {
    pub(super) const fn name(self) -> &'static str {
        match self {
            Self::Sdk2Mid360V0 => "sdk2_mid360_v0",
            Self::Sdk2Mid360sV0 => "sdk2_mid360s_v0",
            Self::Sdk2Mid360lV0 => "sdk2_mid360l_v0",
            Self::Sdk2HapTxV0 => "sdk2_hap_tx_v0",
            Self::Sdk2HapT1V0 => "sdk2_hap_t1_v0",
            Self::LegacyHorizonV5 => "legacy_horizon_v5",
            Self::LegacyAviaV5 => "legacy_avia_v5",
        }
    }
    const fn legacy(self) -> bool {
        matches!(self, Self::LegacyHorizonV5 | Self::LegacyAviaV5)
    }
    const fn hap(self) -> bool {
        matches!(self, Self::Sdk2HapTxV0 | Self::Sdk2HapT1V0)
    }
    const fn sdk2_spherical(self) -> bool {
        matches!(self, Self::Sdk2Mid360V0 | Self::Sdk2Mid360sV0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum StreamKind {
    Points,
    Imu,
}
impl StreamKind {
    pub(super) const fn name(self) -> &'static str {
        match self {
            Self::Points => "points",
            Self::Imu => "imu",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) struct Format {
    pub profile: Profile,
    pub stream: StreamKind,
}

pub(super) fn validate_format(format: Format) -> Result<()> {
    if format.profile == Profile::Sdk2HapT1V0 && format.stream == StreamKind::Imu {
        return Err(unsupported("HAP T1 has no qualified IMU stream"));
    }
    Ok(())
}

pub(super) const fn minimum_packet_bytes(format: Format) -> usize {
    match (format.profile, format.stream) {
        (Profile::LegacyHorizonV5 | Profile::LegacyAviaV5, StreamKind::Imu) => 42,
        (_, StreamKind::Imu) => 60,
        (Profile::LegacyAviaV5, StreamKind::Points) => 678,
        (Profile::LegacyHorizonV5, StreamKind::Points) => 786,
        (_, StreamKind::Points) => 804,
    }
}

pub(super) const fn maximum_packet_bytes(format: Format) -> usize {
    match (format.profile, format.stream) {
        (Profile::LegacyHorizonV5 | Profile::LegacyAviaV5, StreamKind::Imu) => 42,
        (_, StreamKind::Imu) => 60,
        (Profile::LegacyHorizonV5 | Profile::LegacyAviaV5, StreamKind::Points) => 1362,
        (_, StreamKind::Points) => 1380,
    }
}

/// Fixed bound includes every native return, raw header and optional finite float.
pub(super) const fn decoded_json_bound(_format: Format) -> usize {
    MAX_JSON
}

#[derive(Debug)]
pub(super) struct Decoded {
    pub value: Value,
    pub elements: usize,
}

fn malformed(message: &str) -> anyhow::Error {
    Categorized::error(ErrorKind::Incomplete, format!("Livox: {message}"))
}
fn unsupported(message: &str) -> anyhow::Error {
    Categorized::error(ErrorKind::Unsupported, format!("Livox: {message}"))
}
fn span(raw: &[u8], offset: usize, count: usize) -> Result<&[u8]> {
    let end = offset
        .checked_add(count)
        .ok_or_else(|| malformed("field offset overflow"))?;
    raw.get(offset..end)
        .ok_or_else(|| malformed("truncated native field"))
}
fn bytes<const N: usize>(raw: &[u8], offset: usize) -> Result<[u8; N]> {
    span(raw, offset, N)?
        .try_into()
        .map_err(|_| malformed("incorrect native field extent"))
}
fn u16_at(raw: &[u8], offset: usize) -> Result<u16> {
    Ok(u16::from_le_bytes(bytes(raw, offset)?))
}
fn u32_at(raw: &[u8], offset: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(bytes(raw, offset)?))
}
fn u64_at(raw: &[u8], offset: usize) -> Result<u64> {
    Ok(u64::from_le_bytes(bytes(raw, offset)?))
}

struct JsonSize(usize);
impl Write for JsonSize {
    fn write(&mut self, raw: &[u8]) -> io::Result<usize> {
        self.0 = self
            .0
            .checked_add(raw.len())
            .filter(|count| *count <= MAX_JSON)
            .ok_or_else(|| io::Error::other("Livox native JSON exceeds fixed bound"))?;
        Ok(raw.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) fn decode(raw: &[u8], format: Format) -> Result<Decoded> {
    validate_format(format)?;
    let header_bytes = if format.profile.legacy() { 18 } else { 36 };
    span(raw, 0, header_bytes)?;
    let expected_version = if format.profile.legacy() { 5 } else { 0 };
    if raw[0] != expected_version {
        return Err(unsupported(
            "packet version differs from configured profile",
        ));
    }
    let mut decoded = if format.profile.legacy() {
        legacy(raw, format)?
    } else {
        sdk2(raw, format)?
    };
    decoded.value["profile"] = json!(format.profile.name());
    decoded.value["stream"] = json!(format.stream.name());
    decoded.value["packet_version_raw"] = json!(expected_version);
    decoded.value["header_base64"] = json!(STANDARD.encode(span(raw, 0, header_bytes)?));
    serde_json::to_writer(JsonSize(0), &decoded.value)
        .map_err(|_| malformed("bounded native JSON serialization failed"))?;
    Ok(decoded)
}

fn sdk2(raw: &[u8], format: Format) -> Result<Decoded> {
    let header = bytes::<36>(raw, 0)?;
    let data_type = header[10];
    let (records, stride, layout) = match (format.stream, data_type) {
        (StreamKind::Imu, 0) => (1, 24, Layout::Imu),
        (StreamKind::Points, 1) => (96, 14, Layout::Cartesian32(1)),
        (StreamKind::Points, 2) => (96, 8, Layout::Cartesian16),
        (StreamKind::Points, 3) if format.profile.sdk2_spherical() => {
            (96, 10, Layout::Spherical(1))
        }
        _ => {
            return Err(unsupported(
                "SDK2 data type is not qualified for this profile/stream",
            ));
        }
    };
    if raw.len() != 36 + records * stride
        || usize::from(u16_at(raw, 1)?) != raw.len()
        || usize::from(u16_at(raw, 5)?) != records
    {
        return Err(malformed(
            "SDK2 length, count or native record extent is inconsistent",
        ));
    }
    let crc = u32_at(raw, 24)?;
    if crc32fast::hash(span(raw, 28, raw.len() - 28)?) != crc {
        return Err(malformed("SDK2 timestamp/data CRC32 does not match"));
    }
    let mut value = json!({
        "generation":"sdk2_v0",
        "data_type_raw":data_type,
        "packet_length_raw":u16_at(raw, 1)?,
        "time_interval_100ns_raw":u16_at(raw, 3)?,
        "dot_num_raw":u16_at(raw, 5)?,
        "udp_cnt_raw":u16_at(raw, 7)?,
        "frame_cnt_raw":header[9],
        "reserved_base64":STANDARD.encode(span(raw, 12, 12)?),
        "crc32_raw":crc,
        "checksum_status":"crc32_verified",
        "checksum_scope":"timestamp_and_data",
        "clock":sdk2_clock(raw, format.profile)?,
        "native":native(span(raw, 36, raw.len() - 36)?, records, layout)?,
    });
    if format.profile.hap() {
        let pack_info = header[12];
        value["pack_info"] = json!({
            "byte_raw":pack_info,
            "safety_info_raw":pack_info & 3,
            "tag_type_raw":(pack_info >> 2) & 3,
            "reserved_high_nibble_raw":pack_info >> 4,
        });
        value["tag_semantics_status"] = json!("device_reported_not_independently_verified");
    } else {
        value["tag_semantics_status"] = json!("unqualified_header_selector");
    }
    Ok(Decoded {
        value,
        elements: records,
    })
}

fn sdk2_clock(raw: &[u8], profile: Profile) -> Result<Value> {
    let kind = raw[11];
    let stamp = u64_at(raw, 28)?;
    let domain = match kind {
        0 => "device_boot_relative",
        1 => "reported_master_clock",
        2 if !profile.hap() => "reported_gps_clock",
        _ => "unqualified",
    };
    let mut value = json!({
        "timestamp_type_raw":kind,
        "timestamp_base64":STANDARD.encode(span(raw, 28, 8)?),
        "timestamp_word_raw":stamp,
        "domain":domain,
        "synchronization_status":"not_independently_verified",
    });
    if domain != "unqualified" {
        value["nanoseconds_raw"] = json!(stamp);
    }
    Ok(value)
}

#[derive(Clone, Copy)]
enum Layout {
    Cartesian32(usize),
    Cartesian16,
    Spherical(usize),
    Imu,
}

fn legacy(raw: &[u8], format: Format) -> Result<Decoded> {
    let data_type = raw[9];
    let (records, stride, layout, returns) = match (format.stream, data_type) {
        (StreamKind::Imu, 6) => (1, 24, Layout::Imu, 1),
        (StreamKind::Points, 2) => (96, 14, Layout::Cartesian32(1), 1),
        (StreamKind::Points, 3) => (96, 10, Layout::Spherical(1), 1),
        (StreamKind::Points, 4) => (48, 28, Layout::Cartesian32(2), 2),
        (StreamKind::Points, 5) => (48, 16, Layout::Spherical(2), 2),
        (StreamKind::Points, 7) if format.profile == Profile::LegacyAviaV5 => {
            (30, 42, Layout::Cartesian32(3), 3)
        }
        (StreamKind::Points, 8) if format.profile == Profile::LegacyAviaV5 => {
            (30, 22, Layout::Spherical(3), 3)
        }
        _ => {
            return Err(unsupported(
                "legacy data type is not qualified for this profile/stream",
            ));
        }
    };
    if raw.len() != 18 + records * stride {
        return Err(malformed("legacy native record extent is inconsistent"));
    }
    Ok(Decoded {
        value: json!({
            "generation":"legacy_v5",
            "data_type_raw":data_type,
            "slot_raw":raw[1],
            "lidar_id_raw":raw[2],
            "reserved_raw":raw[3],
            "status_raw":u32_at(raw, 4)?,
            "checksum_status":"not_present_in_sample_protocol",
            "checksum_scope":"none",
            "clock":legacy_clock(raw)?,
            "tag_semantics_status":"raw_return_slot_preserved",
            "native":native(span(raw, 18, raw.len() - 18)?, records, layout)?,
        }),
        elements: records * returns,
    })
}

fn legacy_clock(raw: &[u8]) -> Result<Value> {
    let kind = raw[8];
    let stamp = bytes::<8>(raw, 10)?;
    let mut value = json!({
        "timestamp_type_raw":kind,
        "timestamp_base64":STANDARD.encode(stamp),
        "timestamp_word_raw":u64::from_le_bytes(stamp),
        "synchronization_status":"not_independently_verified",
    });
    match kind {
        0 | 1 => {
            value["domain"] = json!(if kind == 0 {
                "device_boot_relative"
            } else {
                "reported_ptp_clock"
            });
            value["nanoseconds_raw"] = json!(u64::from_le_bytes(stamp));
        }
        3 => {
            let [year, month, day, hour, a, b, c, d] = stamp;
            let micros = u32::from_le_bytes([a, b, c, d]);
            let nominal = (1..=12).contains(&month)
                && (1..=31).contains(&day)
                && hour <= 23
                && micros < 3_600_000_000;
            value["domain"] = json!("reported_gps_utc_fields");
            value["utc_fields"] = json!({
                "year_raw":year,
                "month_raw":month,
                "day_raw":day,
                "hour_raw":hour,
                "microseconds_within_hour_raw":micros,
                "field_ranges_status":if nominal { "within_nominal" } else { "out_of_nominal" },
            });
        }
        4 => {
            value["domain"] = json!("reported_pps_relative");
            value["pps_offset_ns_raw"] = json!(i64::from_le_bytes(stamp));
        }
        _ => value["domain"] = json!("unqualified"),
    }
    Ok(value)
}

fn native(raw: &[u8], records: usize, layout: Layout) -> Result<Value> {
    match layout {
        Layout::Cartesian32(returns) => cartesian(raw, records, returns, false),
        Layout::Cartesian16 => cartesian(raw, records, 1, true),
        Layout::Spherical(returns) => spherical(raw, records, returns),
        Layout::Imu => imu(raw),
    }
}

fn cartesian(raw: &[u8], records: usize, returns: usize, low: bool) -> Result<Value> {
    let stride = if low { 8 } else { 14 };
    if raw.len() != records * returns * stride {
        return Err(malformed("Cartesian record extent is inconsistent"));
    }
    let mut x = Vec::with_capacity(records);
    let mut y = Vec::with_capacity(records);
    let mut z = Vec::with_capacity(records);
    let mut reflection = Vec::with_capacity(records);
    let mut tags = Vec::with_capacity(records);
    for record in 0..records {
        let mut row_x = Vec::with_capacity(returns);
        let mut row_y = Vec::with_capacity(returns);
        let mut row_z = Vec::with_capacity(returns);
        let mut row_reflection = Vec::with_capacity(returns);
        let mut row_tags = Vec::with_capacity(returns);
        for slot in 0..returns {
            let block = span(raw, (record * returns + slot) * stride, stride)?;
            let (a, b, c, reflectivity, tag) = if low {
                let [reflectivity, tag] = bytes(block, 6)?;
                (
                    i32::from(i16::from_le_bytes(bytes(block, 0)?)),
                    i32::from(i16::from_le_bytes(bytes(block, 2)?)),
                    i32::from(i16::from_le_bytes(bytes(block, 4)?)),
                    reflectivity,
                    tag,
                )
            } else {
                let [reflectivity, tag] = bytes(block, 12)?;
                (
                    i32::from_le_bytes(bytes(block, 0)?),
                    i32::from_le_bytes(bytes(block, 4)?),
                    i32::from_le_bytes(bytes(block, 8)?),
                    reflectivity,
                    tag,
                )
            };
            row_x.push(a);
            row_y.push(b);
            row_z.push(c);
            row_reflection.push(reflectivity);
            row_tags.push(tag);
        }
        x.push(row_x);
        y.push(row_y);
        z.push(row_z);
        reflection.push(row_reflection);
        tags.push(row_tags);
    }
    Ok(json!({
        "layout":if low { "cartesian_i16" } else { "cartesian_i32" },
        "record_count":records,
        "returns_per_record":returns,
        "coordinate_unit_mm":if low { 10 } else { 1 },
        "x_raw":x,
        "y_raw":y,
        "z_raw":z,
        "reflectivity":reflection,
        "tag_raw":tags,
    }))
}

fn spherical(raw: &[u8], records: usize, returns: usize) -> Result<Value> {
    let stride = 4 + returns * 6;
    if raw.len() != records * stride {
        return Err(malformed("spherical record extent is inconsistent"));
    }
    let mut depth = Vec::with_capacity(records);
    let mut theta = Vec::with_capacity(records);
    let mut phi = Vec::with_capacity(records);
    let mut nominal = Vec::with_capacity(records);
    let mut reflection = Vec::with_capacity(records);
    let mut tags = Vec::with_capacity(records);
    for record in 0..records {
        let block = span(raw, record * stride, stride)?;
        let angle_offset = if returns == 1 { 4 } else { 0 };
        let zenith = u16_at(block, angle_offset)?;
        let azimuth = u16_at(block, angle_offset + 2)?;
        theta.push(zenith);
        phi.push(azimuth);
        nominal.push(zenith <= 18_000 && azimuth <= 36_000);
        let mut row_depth = Vec::with_capacity(returns);
        let mut row_reflection = Vec::with_capacity(returns);
        let mut row_tags = Vec::with_capacity(returns);
        for slot in 0..returns {
            let offset = if returns == 1 { 0 } else { 4 + slot * 6 };
            let [reflectivity, tag] = bytes(block, offset + if returns == 1 { 8 } else { 4 })?;
            row_depth.push(u32_at(block, offset)?);
            row_reflection.push(reflectivity);
            row_tags.push(tag);
        }
        depth.push(row_depth);
        reflection.push(row_reflection);
        tags.push(row_tags);
    }
    Ok(json!({
        "layout":"spherical",
        "record_count":records,
        "returns_per_record":returns,
        "depth_mm":depth,
        "theta_centidegrees_raw":theta,
        "phi_centidegrees_raw":phi,
        "angles_within_nominal":nominal,
        "reflectivity":reflection,
        "tag_raw":tags,
    }))
}

fn float_word(word: u32) -> Value {
    let native = f32::from_bits(word);
    let classification = if native.is_nan() {
        "nan"
    } else if native == f32::INFINITY {
        "positive_infinity"
    } else if native == f32::NEG_INFINITY {
        "negative_infinity"
    } else {
        "finite"
    };
    let mut value = json!({
        "bits":word,
        "classification":classification,
        "sign_bit":word & 0x8000_0000 != 0,
    });
    if native.is_finite() {
        value["value"] = json!(f64::from(native));
    }
    value
}

fn imu(raw: &[u8]) -> Result<Value> {
    if raw.len() != 24 {
        return Err(malformed("IMU requires exactly one 24-byte native record"));
    }
    let gyro = [u32_at(raw, 0)?, u32_at(raw, 4)?, u32_at(raw, 8)?];
    let acceleration = [u32_at(raw, 12)?, u32_at(raw, 16)?, u32_at(raw, 20)?];
    Ok(json!({
        "layout":"imu_float32",
        "record_count":1,
        "gyro_unit":"rad/s",
        "acceleration_unit":"g",
        "gyro_raw_words":gyro,
        "acceleration_raw_words":acceleration,
        "gyro":gyro.map(float_word),
        "acceleration":acceleration.map(float_word),
    }))
}

#[cfg(test)]
mod tests;
