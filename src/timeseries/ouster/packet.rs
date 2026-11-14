//! Native Ouster wire fields, without XYZ, return ranking or clock inference.
//!
//! Manufacturer firmware 3.2 lidar/IMU and LEGACY layouts:
//! <https://docs.ouster.com/sensor-docs/firmware/3.2/lidar-data>
//! <https://docs.ouster.com/sensor-docs/firmware/3.2/lidar-data-legacy>
//! <https://docs.ouster.com/sensor-docs/firmware/3.2/imu-data>
//! <https://docs.ouster.com/sensor-docs/firmware/3.2/imu-data-legacy>
//! Offsets independently checked against official SDK `parsing.cpp` at
//! `46b7bceee7995914e2f3f3facf4d78418335b2d7`; no SDK source is copied.
use crate::types::{Categorized, ErrorKind, Usage};
use anyhow::Result;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::io::{self, Write};

const MAX_UDP: usize = 65_507;
const MAX_JSON: usize = 1 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Profile {
    ModernSingle,
    ModernDual,
    ModernLow,
    ModernLowDual,
    LegacyLidar,
    LegacyImu,
    SynchronousImu,
}
impl Profile {
    pub(super) const fn name(self) -> &'static str {
        match self {
            Self::ModernSingle => "RNG19_RFL8_SIG16_NIR16",
            Self::ModernDual => "RNG19_RFL8_SIG16_NIR16_DUAL",
            Self::ModernLow => "RNG15_RFL8_NIR8",
            Self::ModernLowDual => "RNG15_RFL8_NIR8_DUAL",
            Self::LegacyLidar | Self::LegacyImu => "LEGACY",
            Self::SynchronousImu => "ACCEL32_GYRO32_NMEA",
        }
    }
    pub(super) const fn is_imu(self) -> bool {
        matches!(self, Self::LegacyImu | Self::SynchronousImu)
    }
    const fn stride(self) -> usize {
        match self {
            Self::ModernDual => 16,
            Self::ModernLow => 4,
            Self::ModernLowDual => 8,
            Self::ModernSingle | Self::LegacyLidar => 12,
            Self::LegacyImu | Self::SynchronousImu => 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Integrity {
    Required,
    PreCrcReserved,
    NoCrcInFormat,
}
impl Integrity {
    pub(super) const fn name(self) -> &'static str {
        match self {
            Self::Required => "crc64_required",
            Self::PreCrcReserved => "version_qualified_reserved_footer",
            Self::NoCrcInFormat => "no_checksum_in_format",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) struct Format {
    pub profile: Profile,
    pub integrity: Integrity,
    pub header_features: bool,
    pub pixels_per_column: usize,
    pub columns_per_packet: usize,
    pub columns_per_frame: usize,
    pub serial: Option<u64>,
    pub init_id: Option<u32>,
    pub window: bool,
    pub imu_measurements_per_packet: usize,
}

fn validate(format: &Format) -> Result<()> {
    let legacy = matches!(format.profile, Profile::LegacyLidar | Profile::LegacyImu);
    if legacy {
        if format.integrity != Integrity::NoCrcInFormat || format.header_features {
            return Err(Usage(
                "Ouster LEGACY requires no-checksum format and no standard header features".into(),
            )
            .into());
        }
        if format.serial.is_some() || format.init_id.is_some() || format.window {
            return Err(
                Usage("Ouster LEGACY has no packet identity or window field".into()).into(),
            );
        }
    } else if !matches!(format.serial, Some(serial) if serial < (1_u64 << 40))
        || !matches!(format.init_id, Some(init) if init < (1_u32 << 24))
    {
        return Err(
            Usage("Ouster STANDARD requires explicit u40 serial/u24 init identity".into()).into(),
        );
    }
    if !legacy
        && (format.integrity == Integrity::NoCrcInFormat
            || (format.integrity == Integrity::PreCrcReserved
                && (format.header_features || format.window)))
    {
        return Err(Usage(
            "Ouster STANDARD requires explicit CRC policy and qualified header features".into(),
        )
        .into());
    }
    if format.profile == Profile::SynchronousImu && format.integrity != Integrity::Required {
        return Err(Usage("Ouster synchronous IMU requires its defined CRC64".into()).into());
    }
    if format.profile.is_imu() {
        if format.pixels_per_column != 0
            || format.columns_per_packet != 0
            || format.columns_per_frame != 0
            || format.window
        {
            return Err(
                Usage("Ouster IMU format must not contain lidar dimensions/window".into()).into(),
            );
        }
        let valid = match format.profile {
            Profile::LegacyImu => format.imu_measurements_per_packet == 0,
            Profile::SynchronousImu => (1..=1815).contains(&format.imu_measurements_per_packet),
            _ => false,
        };
        if !valid {
            return Err(Usage(
                "Ouster IMU count must be zero for LEGACY or 1..1815 for synchronous".into(),
            )
            .into());
        }
    } else if format.columns_per_packet != 16
        || !matches!(format.columns_per_frame, 512 | 1024 | 2048)
        || !(matches!(format.pixels_per_column, 32 | 64 | 128)
            || (format.profile == Profile::LegacyLidar && format.pixels_per_column == 16))
        || format.imu_measurements_per_packet != 0
        || (format.profile == Profile::ModernLow && format.window)
    {
        return Err(Usage("Ouster lidar requires C16, W512/1024/2048, H32/64/128 (LEGACY also16), no IMU count and qualified window only on standard single/dual".into()).into());
    }
    Ok(())
}

pub(super) fn expected_packet_bytes(format: &Format) -> Result<usize> {
    validate(format)?;
    let size = match format.profile {
        Profile::LegacyImu => Some(48),
        Profile::SynchronousImu => format
            .imu_measurements_per_packet
            .checked_mul(36)
            .and_then(|n| n.checked_add(164)),
        profile => {
            let legacy = profile == Profile::LegacyLidar;
            format
                .pixels_per_column
                .checked_mul(profile.stride())
                .and_then(|n| n.checked_add(if legacy { 20 } else { 12 }))
                .and_then(|n| n.checked_mul(format.columns_per_packet))
                .and_then(|n| n.checked_add(if legacy { 0 } else { 64 }))
        }
    }
    .filter(|size| *size <= MAX_UDP);
    size.ok_or_else(|| Usage("Ouster configured packet exceeds bounded UDP payload".into()).into())
}

/// Conservative serialized bound, not a claim that all allowed raw sizes fit a group.
pub(super) fn decoded_json_bound(format: &Format) -> Result<usize> {
    expected_packet_bytes(format)?;
    // Fixed header/NMEA/provenance and all column object keys fit the 8KiB reserve.
    // Native lidar integers have at most seven digits per sample; these per-pixel
    // allowances include both code/scaled arrays and separators. One IMU sample
    // has six float objects (each <=96B with <=32B finite-number spelling), plus
    // <=256B for its clocks/correlation/array keys: 6*96+256 = 832.
    Ok(match format.profile {
        Profile::LegacyImu => 4096,
        Profile::SynchronousImu => 8192 + format.imu_measurements_per_packet * 832,
        profile => {
            let per_pixel = match profile {
                Profile::ModernDual => 192,
                Profile::ModernLow => 96,
                _ => 128,
            };
            8192 + format.columns_per_packet * 256
                + format.columns_per_packet * format.pixels_per_column * per_pixel
        }
    })
}

#[derive(Debug)]
pub(super) struct Decoded {
    pub value: Value,
    pub elements: usize,
}

fn malformed(message: &str) -> anyhow::Error {
    Categorized::error(ErrorKind::OperationFailed, format!("Ouster: {message}"))
}
fn bytes<const N: usize>(raw: &[u8], offset: usize) -> Result<[u8; N]> {
    let end = offset
        .checked_add(N)
        .ok_or_else(|| malformed("field offset overflow"))?;
    raw.get(offset..end)
        .ok_or_else(|| malformed("truncated field"))?
        .try_into()
        .map_err(|_| malformed("invalid field extent"))
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

const fn crc_table() -> [u64; 256] {
    let mut table = [0; 256];
    let mut index = 0;
    while index < table.len() {
        let mut value = index as u64;
        let mut bit = 0;
        while bit < 8 {
            value = if value & 1 == 0 {
                value >> 1
            } else {
                (value >> 1) ^ 0xc96c_5795_d787_0f42
            };
            bit += 1;
        }
        table[index] = value;
        index += 1;
    }
    table
}
const CRC_TABLE: [u64; 256] = crc_table();
fn crc64(raw: &[u8]) -> u64 {
    let mut crc = u64::MAX;
    for &byte in raw {
        let [low, ..] = crc.to_le_bytes();
        let index = usize::from(low ^ byte);
        assert!(index < CRC_TABLE.len());
        crc = CRC_TABLE[index] ^ (crc >> 8);
    }
    !crc
}

fn header(
    raw: &[u8],
    expected_serial: u64,
    expected_init: u32,
    packet_type: u16,
    features: bool,
) -> Result<Value> {
    let header = bytes::<32>(raw, 0)?;
    let actual_type = u16_at(&header, 0)?;
    if actual_type != packet_type {
        return Err(malformed(
            "STANDARD packet type differs from configured stream",
        ));
    }
    let init = u32_at(&header, 4)? & 0x00ff_ffff;
    let serial = u64_at(&header, 4)? >> 24;
    if serial != expected_serial {
        return Err(Categorized::new(
            ErrorKind::OperationFailed,
            "Ouster packet serial differs from metadata",
        )
        .details(json!({"expected_serial":expected_serial,"actual_serial":serial}))
        .into());
    }
    if init != expected_init {
        return Err(Categorized::new(ErrorKind::Disconnected, "Ouster packet init ID changed from metadata epoch")
            .details(json!({"reason":"ouster_init_id_changed","expected_init_id":expected_init,"actual_init_id":init})).into());
    }
    let mut value = json!({"packet_type_raw":actual_type,"frame_id_raw":u16_at(&header,2)?,
        "init_id_raw":init,"serial_raw":serial});
    if features {
        let fields = json!({"alert_flags_raw":header[12],"alert_cursors_raw":header[12]&63,
            "alert_cursor_overflow":header[12]&64!=0,"alerts_active":header[12]&128!=0,
            "thermal_shutdown_countdown_raw":header[16],"shot_limiting_countdown_raw":header[17],
            "thermal_shutdown_state_raw":header[18],"thermal_shutdown_state":header[18]&15,
            "shot_limiting_state_raw":header[19],"shot_limiting_level":header[19]&15});
        for (key, field) in fields
            .as_object()
            .ok_or_else(|| malformed("invalid header fields"))?
        {
            value[key].clone_from(field);
        }
    } else {
        value["status_decoding"] = json!("unavailable_version_qualified");
        value["status_bytes_base64"] = json!(STANDARD.encode(&header[12..]));
    }
    Ok(value)
}

fn standard(
    raw: &[u8],
    serial: u64,
    init_id: u32,
    packet_type: u16,
    integrity: Integrity,
    features: bool,
) -> Result<(Value, Option<u64>)> {
    if serial >= (1_u64 << 40)
        || init_id >= (1_u32 << 24)
        || !matches!(packet_type, 1..=3)
        || integrity == Integrity::NoCrcInFormat
        || (integrity == Integrity::PreCrcReserved && features)
    {
        return Err(
            Usage("Ouster STANDARD identity/type/integrity exceeds defined bounds".into()).into(),
        );
    }
    if !(64..=MAX_UDP).contains(&raw.len()) {
        return Err(malformed("STANDARD packet size must be 64..65507 bytes"));
    }
    let encoded = if integrity == Integrity::Required {
        let checksum_offset = raw.len() - 8;
        let encoded = u64_at(raw, checksum_offset)?;
        let calculated = crc64(
            raw.get(..checksum_offset)
                .ok_or_else(|| malformed("missing CRC input"))?,
        );
        if encoded != calculated {
            return Err(malformed("CRC64 differs from encoded footer"));
        }
        Some(encoded)
    } else {
        None
    };
    Ok((
        header(raw, serial, init_id, packet_type, features)?,
        encoded,
    ))
}

/// Header/identity and declared integrity proof only; no body-profile decoding.
pub(super) fn validate_standard_packet(
    raw: &[u8],
    serial: u64,
    init_id: u32,
    packet_type: u16,
    integrity: Integrity,
) -> Result<()> {
    standard(raw, serial, init_id, packet_type, integrity, false).map(|_| ())
}

struct JsonBudget(usize);
impl Write for JsonBudget {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let next = self
            .0
            .checked_add(bytes.len())
            .filter(|n| *n <= MAX_JSON)
            .ok_or_else(|| io::Error::other("Ouster decoded packet exceeds 1 MiB"))?;
        self.0 = next;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl JsonBudget {
    fn add(&mut self, value: &Value) -> Result<()> {
        serde_json::to_writer(self, value).map_err(|_| malformed("decoded packet exceeds 1 MiB"))
    }
    fn separator(&mut self, used: bool) -> Result<()> {
        if used {
            self.write_all(b",")
                .map_err(|_| malformed("decoded packet exceeds 1 MiB"))?;
        }
        Ok(())
    }
}

pub(super) fn decode(raw: &[u8], format: &Format) -> Result<Decoded> {
    let expected = expected_packet_bytes(format)?;
    if raw.len() != expected {
        return Err(malformed("UDP payload size differs from configured format"));
    }
    if format.profile == Profile::LegacyImu {
        return legacy_imu(raw, format);
    }
    let mut value = json!({"kind":if format.profile.is_imu() {"ouster_imu_packet"}else{"ouster_lidar_packet"},
        "profile":format.profile.name(),"packet_bytes":raw.len(),"clock_basis":"unspecified"});
    if format.profile != Profile::LegacyLidar {
        let serial = format
            .serial
            .ok_or_else(|| malformed("missing configured serial"))?;
        let init = format
            .init_id
            .ok_or_else(|| malformed("missing configured init ID"))?;
        let (header, encoded) = standard(
            raw,
            serial,
            init,
            if format.profile.is_imu() { 2 } else { 1 },
            format.integrity,
            format.header_features,
        )?;
        value["header"] = header;
        if let Some(encoded) = encoded {
            value["crc64_raw"] = json!(encoded);
            value["crc64_valid"] = json!(true);
        } else {
            value["checksum_status"] = json!("unavailable_version_qualified");
            let footer = raw
                .get(raw.len() - 32..)
                .ok_or_else(|| malformed("missing reserved footer"))?;
            value["footer_base64"] = json!(STANDARD.encode(footer));
        }
    }
    if format.profile == Profile::SynchronousImu {
        return synchronous_imu(raw, format, value);
    }
    value["pixels_per_column"] = json!(format.pixels_per_column);
    value["columns_per_packet"] = json!(format.columns_per_packet);
    value["columns_per_frame"] = json!(format.columns_per_frame);
    value["columns"] = json!([]);
    let mut budget = JsonBudget(0);
    budget.add(&value)?;
    let legacy = format.profile == Profile::LegacyLidar;
    let stride = format.pixels_per_column * format.profile.stride() + if legacy { 20 } else { 12 };
    let offset = if legacy { 0 } else { 32 };
    let mut columns = Vec::with_capacity(format.columns_per_packet);
    for index in 0..format.columns_per_packet {
        let start = offset + index * stride;
        let raw_column = raw
            .get(start..start + stride)
            .ok_or_else(|| malformed("truncated column"))?;
        let column = column(raw_column, format)?;
        budget.separator(!columns.is_empty())?;
        budget.add(&column)?;
        columns.push(column);
    }
    value["columns"] = Value::Array(columns);
    Ok(Decoded {
        value,
        elements: format.columns_per_packet * format.pixels_per_column,
    })
}

fn column(raw: &[u8], format: &Format) -> Result<Value> {
    let legacy = format.profile == Profile::LegacyLidar;
    let status = if legacy {
        u32_at(raw, raw.len() - 4)?
    } else {
        u32::from(u16_at(raw, 10)?)
    };
    let status_known = !legacy || matches!(status, 0 | u32::MAX);
    let valid = if legacy {
        status == u32::MAX
    } else {
        status & 1 != 0
    };
    let measurement = u16_at(raw, 8)?;
    let in_bounds = usize::from(measurement) < format.columns_per_frame;
    if valid && !in_bounds {
        return Err(malformed(
            "valid column measurement ID exceeds configured frame width",
        ));
    }
    let validity = match (status_known, valid) {
        (false, _) => "unknown",
        (true, true) => "valid",
        (true, false) => "invalid",
    };
    let mut value = json!({"timestamp_ns_raw":u64_at(raw,0)?,"measurement_id_raw":measurement,
        "measurement_id_in_bounds":in_bounds,"status_raw":status,"status_known":status_known,"validity":validity});
    if status_known {
        value["valid"] = json!(valid);
    }
    if legacy {
        let encoder = u32_at(raw, 12)?;
        let ticks = encoder & 0x00ff_ffff;
        let encoder_in_bounds = ticks < 90_112;
        if valid && !encoder_in_bounds {
            return Err(malformed("valid LEGACY encoder exceeds documented range"));
        }
        value["frame_id_raw"] = json!(u16_at(raw, 10)?);
        value["encoder_word_raw"] = json!(encoder);
        value["encoder_ticks_raw"] = json!(ticks);
        value["encoder_in_bounds"] = json!(encoder_in_bounds);
    }
    let mut range = Vec::with_capacity(format.pixels_per_column);
    let mut millimeters = Vec::with_capacity(format.pixels_per_column);
    let mut flags = Vec::with_capacity(format.pixels_per_column);
    let mut reflectivity = Vec::with_capacity(format.pixels_per_column);
    let mut signal = Vec::with_capacity(format.pixels_per_column);
    let mut near_raw = Vec::with_capacity(format.pixels_per_column);
    let mut near = Vec::with_capacity(format.pixels_per_column);
    let mut range2 = Vec::with_capacity(format.pixels_per_column);
    let mut millimeters2 = Vec::with_capacity(format.pixels_per_column);
    let mut flags2 = Vec::with_capacity(format.pixels_per_column);
    let mut reflectivity2 = Vec::with_capacity(format.pixels_per_column);
    let mut signal2 = Vec::with_capacity(format.pixels_per_column);
    let mut window = Vec::with_capacity(format.pixels_per_column);
    for index in 0..format.pixels_per_column {
        let start = if legacy { 16 } else { 12 } + index * format.profile.stride();
        let pixel = raw
            .get(start..start + format.profile.stride())
            .ok_or_else(|| malformed("truncated channel"))?;
        let first = u32_at(pixel, 0)?;
        match format.profile {
            Profile::ModernLow => {
                let code = first & 0x7fff;
                range.push(code);
                millimeters.push(code * 8);
                flags.push((first >> 15) & 1);
                reflectivity.push((first >> 16) & 255);
                let infrared = first >> 24;
                near_raw.push(infrared);
                near.push(infrared * 16);
            }
            Profile::ModernLowDual => {
                let code = first & 0x7fff;
                range.push(code);
                millimeters.push(code * 8);
                flags.push((first >> 15) & 1);
                reflectivity.push((first >> 16) & 255);
                let infrared = first >> 24;
                near_raw.push(infrared);
                near.push(infrared * 16);
                let second = u32_at(pixel, 4)?;
                range2.push(second & 0x7fff);
                millimeters2.push((second & 0x7fff) * 8);
                flags2.push((second >> 15) & 1);
                reflectivity2.push((second >> 16) & 255);
                if format.window {
                    window.push(bytes::<1>(pixel, 7)?[0]);
                }
            }
            Profile::ModernDual => {
                let code = first & 0x0007_ffff;
                range.push(code);
                millimeters.push(code);
                flags.push((first >> 19) & 31);
                reflectivity.push(first >> 24);
                let second = u32_at(pixel, 4)?;
                range2.push(second & 0x0007_ffff);
                millimeters2.push(second & 0x0007_ffff);
                flags2.push((second >> 19) & 31);
                reflectivity2.push(second >> 24);
                signal.push(u16_at(pixel, 8)?);
                signal2.push(u16_at(pixel, 10)?);
                let infrared = u32::from(u16_at(pixel, 12)?);
                near_raw.push(infrared);
                near.push(infrared);
                if format.window {
                    window.push(bytes::<1>(pixel, 15)?[0]);
                }
            }
            Profile::ModernSingle | Profile::LegacyLidar => {
                let code = first & if legacy { 0x000f_ffff } else { 0x0007_ffff };
                range.push(code);
                millimeters.push(code);
                flags.push(if legacy {
                    first >> 28
                } else {
                    (first >> 19) & 31
                });
                reflectivity.push(u32_at(pixel, 4)? & 255);
                signal.push(u16_at(pixel, 6)?);
                let infrared = u32::from(u16_at(pixel, 8)?);
                near_raw.push(infrared);
                near.push(infrared);
                if format.window {
                    window.push(bytes::<1>(pixel, 11)?[0]);
                }
            }
            Profile::LegacyImu | Profile::SynchronousImu => {
                return Err(malformed("IMU profile in lidar channel"));
            }
        }
    }
    value["range_raw"] = json!(range);
    value["range_mm"] = json!(millimeters);
    value["flags_raw"] = json!(flags);
    value["reflectivity_raw"] = json!(reflectivity);
    value["near_ir_raw"] = json!(near_raw);
    value["near_ir"] = json!(near);
    if !matches!(format.profile, Profile::ModernLow | Profile::ModernLowDual) {
        value["signal_raw"] = json!(signal);
    }
    if matches!(format.profile, Profile::ModernDual | Profile::ModernLowDual) {
        value["range2_mm"] = json!(millimeters2);
        value["range2_raw"] = json!(range2);
        value["flags2_raw"] = json!(flags2);
        value["reflectivity2_raw"] = json!(reflectivity2);
        if format.profile == Profile::ModernDual {
            value["signal2_raw"] = json!(signal2);
        }
    }
    if format.window {
        value["window_raw"] = json!(window);
    }
    Ok(value)
}

fn ieee(raw: &[u8], offset: usize) -> Result<Value> {
    let word = u32_at(raw, offset)?;
    let number = f32::from_bits(word);
    let state = if number.is_nan() {
        "nan"
    } else if number == f32::INFINITY {
        "positive_infinity"
    } else if number == f32::NEG_INFINITY {
        "negative_infinity"
    } else {
        "finite"
    };
    let mut value = json!({"ieee754_raw":word,"status":state});
    if number.is_finite() {
        value["value"] = json!(f64::from(number));
    }
    Ok(value)
}
fn axes(raw: &[u8], offset: usize) -> Result<Value> {
    Ok(json!([
        ieee(raw, offset)?,
        ieee(raw, offset + 4)?,
        ieee(raw, offset + 8)?
    ]))
}
fn legacy_imu(raw: &[u8], format: &Format) -> Result<Decoded> {
    let value = json!({"kind":"ouster_imu_packet","profile":format.profile.name(),
        "packet_bytes":raw.len(),"clock_basis":"unspecified",
        "diagnostic_timestamp_ns_raw":u64_at(raw,0)?,"accel_timestamp_ns_raw":u64_at(raw,8)?,
        "gyro_timestamp_ns_raw":u64_at(raw,16)?,"acceleration_g":axes(raw,24)?,
        "angular_velocity_dps":axes(raw,36)?});
    let mut budget = JsonBudget(0);
    budget.add(&value)?;
    Ok(Decoded { value, elements: 1 })
}
fn synchronous_imu(raw: &[u8], format: &Format, mut value: Value) -> Result<Decoded> {
    let sentence = raw
        .get(40..128)
        .ok_or_else(|| malformed("truncated NMEA sentence"))?;
    let locks = u32_at(raw, 128)?;
    value["nmea"] = json!({"timestamp_ns_raw":u64_at(raw,32)?,"sentence_bytes":88,
        "sentence_base64":STANDARD.encode(sentence),"lock_word_raw":locks,
        "pps_locked":locks&1!=0,"nmea_locked":locks&2!=0});
    value["correlation_frame_decoding"] = json!("unavailable");
    // Firmware manuals and the pinned SDK disagree about synchronous wire units.
    // Retain exact readings without choosing either interpretation by magnitude.
    value["units_status"] = json!("unresolved_vendor_documentation");
    value["documented_wire_units"] =
        json!({"acceleration":"g","angular_velocity":"degrees_per_second"});
    value["sdk_interpreted_units"] =
        json!({"acceleration":"m/s2","angular_velocity":"radians_per_second"});
    value["measurements"] = json!([]);
    let mut budget = JsonBudget(0);
    budget.add(&value)?;
    let mut measurements = Vec::with_capacity(format.imu_measurements_per_packet);
    for index in 0..format.imu_measurements_per_packet {
        let start = 132 + index * 36;
        let sample = raw
            .get(start..start + 36)
            .ok_or_else(|| malformed("truncated synchronous IMU sample"))?;
        let correlation = u32_at(sample, 8)?;
        let measurement = json!({"timestamp_ns_raw":u64_at(sample,0)?,"correlation_word_raw":correlation,
            "column_id_raw":correlation&0xfff,"result_valid":correlation&(1<<16)!=0,
            "acceleration_wire":axes(sample,12)?,"angular_velocity_wire":axes(sample,24)?});
        budget.separator(!measurements.is_empty())?;
        budget.add(&measurement)?;
        measurements.push(measurement);
    }
    value["measurements"] = Value::Array(measurements);
    Ok(Decoded {
        value,
        elements: format.imu_measurements_per_packet,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    const SERIAL: u64 = 0x0001_0203_0405;
    const INIT: u32 = 0x0006_0708;
    const HEADER: [u8; 32] = [
        1, 0, 0xb2, 0xa1, 8, 7, 6, 5, 4, 3, 2, 1, 0xe5, 0xaa, 0xbb, 0xcc, 30, 17, 0xa1, 0xb9, 0x5a,
        0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a,
    ];
    const IEEE_WORDS: [u32; 6] = [
        0x3f80_0000,
        0x8000_0000,
        0x7fc1_2345,
        0x7f80_0000,
        0xff80_0000,
        1,
    ];
    fn format(profile: Profile) -> Format {
        let legacy = matches!(profile, Profile::LegacyLidar | Profile::LegacyImu);
        let imu = profile.is_imu();
        Format {
            profile,
            integrity: if legacy {
                Integrity::NoCrcInFormat
            } else {
                Integrity::Required
            },
            header_features: !legacy,
            pixels_per_column: if imu { 0 } else { 32 },
            columns_per_packet: if imu { 0 } else { 16 },
            columns_per_frame: if imu { 0 } else { 512 },
            serial: (!legacy).then_some(SERIAL),
            init_id: (!legacy).then_some(INIT),
            window: matches!(
                profile,
                Profile::ModernSingle | Profile::ModernDual | Profile::ModernLowDual
            ),
            imu_measurements_per_packet: if profile == Profile::SynchronousImu {
                2
            } else {
                0
            },
        }
    }
    fn put(raw: &mut [u8], offset: usize, data: &[u8]) {
        let end = offset.checked_add(data.len()).unwrap();
        let Some(target) = raw.get_mut(offset..end) else {
            panic!("literal fixture exceeds declared extent")
        };
        target.copy_from_slice(data);
    }
    // Independent fixed literal templates; CRCs were generated once with MSB-first
    // polynomial/reflected input/output math, not this module's lookup table.
    fn modern_fixture(profile: Profile) -> (Format, Vec<u8>) {
        let (size, stride, pixel, checksum): (usize, usize, &[u8], u64) = match profile {
            Profile::ModernSingle => (
                6400,
                12,
                &[
                    0x45, 0x23, 0x9a, 0xfe, 0xbc, 0xad, 0x34, 0x12, 0x78, 0x56, 0xee, 0xdc,
                ],
                0x6059_afa4_b25c_9f0e,
            ),
            Profile::ModernDual => (
                8448,
                16,
                &[
                    0x45, 0x23, 0x9a, 0xbc, 0xef, 0xcd, 0xab, 0xde, 0x34, 0x12, 0x78, 0x56, 0xbc,
                    0x9a, 0xee, 0xdc,
                ],
                0x547a_104f_1f18_c010,
            ),
            Profile::ModernLow => (2304, 4, &[0xff, 0xff, 0xab, 0xcd], 0xc6fa_49a5_df02_2a12),
            Profile::ModernLowDual => (
                4352,
                8,
                &[0xff, 0xff, 0xab, 0xcd, 0x34, 0x92, 0xef, 0x78],
                0xc17d_c3e8_dbd7_ffea,
            ),
            _ => panic!("literal lidar profile required"),
        };
        let mut raw = vec![0; size];
        put(&mut raw, 0, &HEADER);
        for index in 0..16 {
            let start = 32 + index * (12 + 32 * stride);
            put(
                &mut raw,
                start,
                &(u64::MAX - u64::try_from(index).unwrap()).to_le_bytes(),
            );
            put(
                &mut raw,
                start + 8,
                &if index == 1 {
                    u16::MAX
                } else {
                    u16::try_from(index).unwrap()
                }
                .to_le_bytes(),
            );
            put(
                &mut raw,
                start + 10,
                &if index == 1 { 0xa5a0_u16 } else { 0xa5a1 }.to_le_bytes(),
            );
            put(&mut raw, start + 12, pixel);
        }
        put(&mut raw, size - 8, &checksum.to_le_bytes());
        (format(profile), raw)
    }
    fn legacy_fixture() -> (Format, Vec<u8>) {
        let mut configured = format(Profile::LegacyLidar);
        configured.pixels_per_column = 16;
        let mut raw = vec![0; 3392];
        for index in 0..16 {
            let start = index * 212;
            put(
                &mut raw,
                start,
                &(u64::MAX - u64::try_from(index).unwrap()).to_le_bytes(),
            );
            put(
                &mut raw,
                start + 8,
                &if matches!(index, 1 | 2) {
                    u16::MAX
                } else {
                    u16::try_from(index).unwrap()
                }
                .to_le_bytes(),
            );
            put(
                &mut raw,
                start + 10,
                &(u16::MAX - u16::try_from(index).unwrap()).to_le_bytes(),
            );
            put(
                &mut raw,
                start + 12,
                &if matches!(index, 1 | 2) {
                    u32::MAX
                } else {
                    0xab00_1234
                }
                .to_le_bytes(),
            );
            put(
                &mut raw,
                start + 16,
                &[
                    0x45, 0x23, 0xfa, 0xde, 0xbc, 0xad, 0x34, 0x12, 0x78, 0x56, 0xee, 0xdc,
                ],
            );
            let status = match index {
                1 => 0,
                2 => 0x1234_5678,
                _ => u32::MAX,
            };
            put(&mut raw, start + 208, &status.to_le_bytes());
        }
        (configured, raw)
    }
    fn legacy_imu_fixture() -> Vec<u8> {
        let mut raw = vec![0; 48];
        for (index, clock) in [u64::MAX, 0x0123_4567_89ab_cdef, 0].into_iter().enumerate() {
            put(&mut raw, index * 8, &clock.to_le_bytes());
        }
        for (index, word) in IEEE_WORDS.into_iter().enumerate() {
            put(&mut raw, 24 + index * 4, &word.to_le_bytes());
        }
        raw
    }
    fn sync_fixture() -> (Format, Vec<u8>) {
        let mut raw = vec![0; 236];
        let mut header = HEADER;
        header[0] = 2;
        put(&mut raw, 0, &header);
        put(&mut raw, 32, &(u64::MAX - 1).to_le_bytes());
        let mut nmea = [0xa5; 88];
        nmea[..8].copy_from_slice(b"\xff$GPRMC\0");
        put(&mut raw, 40, &nmea);
        put(&mut raw, 128, &0xa5a5_0003_u32.to_le_bytes());
        for (index, correlation) in [0xabcd_0123_u32, 0x1200_0fff].into_iter().enumerate() {
            let start = 132 + index * 36;
            put(
                &mut raw,
                start,
                &(u64::MAX - u64::try_from(index).unwrap()).to_le_bytes(),
            );
            put(&mut raw, start + 8, &correlation.to_le_bytes());
            for (axis, word) in IEEE_WORDS.into_iter().enumerate() {
                put(&mut raw, start + 12 + axis * 4, &word.to_le_bytes());
            }
        }
        put(&mut raw, 228, &0xf238_c88c_33ee_f151_u64.to_le_bytes());
        (format(Profile::SynchronousImu), raw)
    }
    fn check_ieee(value: &Value, acceleration: &str, angular_velocity: &str) {
        assert_eq!(value[acceleration][0]["ieee754_raw"], 0x3f80_0000);
        assert_eq!(value[acceleration][0]["value"], 1.0);
        assert_eq!(value[acceleration][1]["ieee754_raw"], 0x8000_0000_u32);
        assert!(
            value[acceleration][1]["value"]
                .as_f64()
                .unwrap()
                .is_sign_negative()
        );
        assert_eq!(value[acceleration][2]["ieee754_raw"], 0x7fc1_2345);
        assert_eq!(value[acceleration][2]["status"], "nan");
        assert!(value[acceleration][2].get("value").is_none());
        for (index, state) in [(0, "positive_infinity"), (1, "negative_infinity")] {
            assert_eq!(value[angular_velocity][index]["status"], state);
            assert!(value[angular_velocity][index].get("value").is_none());
        }
        assert_eq!(value[angular_velocity][2]["ieee754_raw"], 1);
        assert_eq!(value[angular_velocity][2]["status"], "finite");
        assert!(value[angular_velocity][2]["value"].as_f64().unwrap() > 0.0);
    }

    #[test]
    fn literal_standard_profiles_keep_dense_fields_raw_codes_and_scaled_units() {
        for profile in [
            Profile::ModernSingle,
            Profile::ModernDual,
            Profile::ModernLow,
            Profile::ModernLowDual,
        ] {
            let (configured, raw) = modern_fixture(profile);
            let decoded = decode(&raw, &configured).unwrap();
            assert_eq!(decoded.elements, 512);
            assert_eq!(decoded.value["profile"], profile.name());
            assert_eq!(decoded.value["crc64_valid"], true);
            let header = &decoded.value["header"];
            assert_eq!(header["serial_raw"], SERIAL);
            assert_eq!(header["init_id_raw"], INIT);
            assert_eq!(header["frame_id_raw"], 0xa1b2);
            assert_eq!(header["alert_cursors_raw"], 37);
            assert_eq!(header["thermal_shutdown_state_raw"], 0xa1);
            assert_eq!(header["thermal_shutdown_state"], 1);
            assert_eq!(header["shot_limiting_level"], 9);
            let columns = decoded.value["columns"].as_array().unwrap();
            assert_eq!(columns.len(), 16);
            let first = &columns[0];
            assert_eq!(first["timestamp_ns_raw"].as_u64().unwrap(), u64::MAX);
            assert_eq!(first["valid"], true);
            assert_eq!(first["range_raw"].as_array().unwrap().len(), 32);
            assert_eq!(first["range_raw"][1], 0);
            assert_eq!(first["range_mm"][1], 0);
            assert_eq!(columns[1]["valid"], false);
            assert_eq!(columns[1]["measurement_id_raw"], u16::MAX);
            assert_eq!(columns[1]["measurement_id_in_bounds"], false);
            assert_eq!(columns[1]["range_raw"], first["range_raw"]);
            if matches!(profile, Profile::ModernLow | Profile::ModernLowDual) {
                assert_eq!(first["range_raw"][0], 32767);
                assert_eq!(first["range_mm"][0], 262_136);
                assert_eq!(first["near_ir_raw"][0], 205);
                assert_eq!(first["near_ir"][0], 3280);
                assert!(first.get("signal_raw").is_none());
            } else {
                assert_eq!(first["range_raw"][0], 140_101);
                assert_eq!(first["range_mm"][0], 140_101);
                assert_eq!(first["flags_raw"][0], 19);
                assert_eq!(first["signal_raw"][0], 4660);
            }
            match profile {
                Profile::ModernSingle => {
                    assert_eq!(first["near_ir_raw"][0], 22_136);
                    assert_eq!(first["window_raw"][0], 220);
                }
                Profile::ModernDual => {
                    assert_eq!(first["range2_raw"][0], 249_327);
                    assert_eq!(first["range2_mm"][0], 249_327);
                    assert_eq!(first["flags2_raw"][0], 21);
                    assert_eq!(first["reflectivity2_raw"][0], 222);
                    assert_eq!(first["signal2_raw"][0], 22_136);
                    assert_eq!(first["near_ir_raw"][0], 39_612);
                }
                Profile::ModernLowDual => {
                    assert_eq!(first["range2_raw"][0], 4660);
                    assert_eq!(first["range2_mm"][0], 37_280);
                    assert_eq!(first["reflectivity2_raw"][0], 239);
                    assert_eq!(first["window_raw"][0], 120);
                    assert!(first.get("signal2_raw").is_none());
                }
                _ => assert!(first.get("range2_raw").is_none()),
            }
            assert!(
                serde_json::to_vec(&decoded.value).unwrap().len()
                    <= decoded_json_bound(&configured).unwrap()
            );
        }
    }

    #[test]
    fn legacy_unknown_and_padded_columns_keep_values_without_inventing_validity() {
        let (configured, raw) = legacy_fixture();
        let decoded = decode(&raw, &configured).unwrap();
        assert_eq!(decoded.elements, 256);
        assert!(decoded.value.get("crc64_valid").is_none());
        let columns = decoded.value["columns"].as_array().unwrap();
        assert_eq!(columns.len(), 16);
        assert_eq!(columns[0]["frame_id_raw"], u16::MAX);
        assert_eq!(columns[0]["flags_raw"][0], 13);
        assert_eq!(columns[0]["range_raw"][0], 664_389);
        assert_eq!(columns[0]["encoder_word_raw"], 0xab00_1234_u32);
        assert_eq!(columns[0]["encoder_ticks_raw"], 4660);
        assert_eq!(columns[1]["validity"], "invalid");
        assert_eq!(columns[1]["measurement_id_in_bounds"], false);
        assert_eq!(columns[1]["encoder_in_bounds"], false);
        assert_eq!(columns[2]["status_raw"], 0x1234_5678);
        assert_eq!(columns[2]["status_known"], false);
        assert_eq!(columns[2]["validity"], "unknown");
        assert!(columns[2].get("valid").is_none());
        assert_eq!(columns[2]["range_raw"], columns[0]["range_raw"]);
        assert!(
            serde_json::to_vec(&decoded.value).unwrap().len()
                <= decoded_json_bound(&configured).unwrap()
        );
    }

    #[test]
    fn legacy_imu_keeps_three_exact_clocks_and_all_ieee_states() {
        let configured = format(Profile::LegacyImu);
        let raw = legacy_imu_fixture();
        let decoded = decode(&raw, &configured).unwrap();
        assert_eq!(decoded.elements, 1);
        assert_eq!(decoded.value["clock_basis"], "unspecified");
        assert_eq!(
            decoded.value["diagnostic_timestamp_ns_raw"]
                .as_u64()
                .unwrap(),
            u64::MAX
        );
        assert_eq!(
            decoded.value["accel_timestamp_ns_raw"].as_u64().unwrap(),
            0x0123_4567_89ab_cdef
        );
        assert_eq!(decoded.value["gyro_timestamp_ns_raw"], 0);
        check_ieee(&decoded.value, "acceleration_g", "angular_velocity_dps");
        assert!(
            serde_json::to_vec(&decoded.value).unwrap().len()
                <= decoded_json_bound(&configured).unwrap()
        );
    }

    #[test]
    fn synchronous_imu_preserves_binary_nmea_raw_correlation_and_dense_ieee_samples() {
        let (configured, raw) = sync_fixture();
        let decoded = decode(&raw, &configured).unwrap();
        assert_eq!(decoded.elements, 2);
        assert_eq!(decoded.value["crc64_valid"], true);
        assert_eq!(decoded.value["header"]["packet_type_raw"], 2);
        assert_eq!(
            decoded.value["nmea"]["timestamp_ns_raw"].as_u64().unwrap(),
            u64::MAX - 1
        );
        assert_eq!(decoded.value["nmea"]["lock_word_raw"], 0xa5a5_0003_u32);
        assert_eq!(
            STANDARD
                .decode(decoded.value["nmea"]["sentence_base64"].as_str().unwrap())
                .unwrap(),
            raw[40..128]
        );
        assert_eq!(decoded.value["correlation_frame_decoding"], "unavailable");
        assert_eq!(
            decoded.value["units_status"],
            "unresolved_vendor_documentation"
        );
        let measurements = decoded.value["measurements"].as_array().unwrap();
        assert_eq!(measurements.len(), 2);
        assert_eq!(measurements[0]["correlation_word_raw"], 0xabcd_0123_u32);
        assert_eq!(measurements[0]["column_id_raw"], 0x123);
        assert_eq!(measurements[0]["result_valid"], true);
        assert_eq!(measurements[1]["result_valid"], false);
        assert_eq!(measurements[1]["column_id_raw"], 4095);
        for sample in measurements {
            check_ieee(sample, "acceleration_wire", "angular_velocity_wire");
            assert!(sample.get("frame_id").is_none());
        }
        assert!(
            serde_json::to_vec(&decoded.value).unwrap().len()
                <= decoded_json_bound(&configured).unwrap()
        );
    }

    #[test]
    fn crc_polynomial_check_and_required_integrity_never_use_zero_footer_fallback() {
        assert_eq!(crc64(b"123456789"), 0x995d_c9bb_df19_39fa);
        assert_eq!(crc64(b""), 0);
        for profile in [
            Profile::ModernSingle,
            Profile::ModernDual,
            Profile::ModernLow,
            Profile::ModernLowDual,
        ] {
            let (configured, mut raw) = modern_fixture(profile);
            validate_standard_packet(&raw, SERIAL, INIT, 1, Integrity::Required).unwrap();
            let end = raw.len();
            put(&mut raw, end - 8, &[0; 8]);
            assert!(
                decode(&raw, &configured)
                    .unwrap_err()
                    .to_string()
                    .contains("CRC64")
            );
            assert!(
                validate_standard_packet(&raw, SERIAL, INIT, 1, Integrity::Required)
                    .unwrap_err()
                    .to_string()
                    .contains("CRC64")
            );
        }
        let (configured, mut raw) = sync_fixture();
        assert!(raw.len() > 144);
        raw[144] ^= 1;
        assert!(
            decode(&raw, &configured)
                .unwrap_err()
                .to_string()
                .contains("CRC64")
        );
    }

    #[test]
    fn version_qualified_reserved_footer_keeps_raw_bytes_without_claiming_checksum_or_flags() {
        let (mut configured, mut raw) = modern_fixture(Profile::ModernDual);
        configured.integrity = Integrity::PreCrcReserved;
        configured.header_features = false;
        configured.window = false;
        let end = raw.len();
        put(&mut raw, end - 32, &[0xa5; 32]);
        let decoded = decode(&raw, &configured).unwrap();
        assert_eq!(
            decoded.value["checksum_status"],
            "unavailable_version_qualified"
        );
        assert_eq!(
            STANDARD
                .decode(decoded.value["footer_base64"].as_str().unwrap())
                .unwrap(),
            [0xa5; 32]
        );
        assert!(decoded.value.get("crc64_raw").is_none());
        assert!(decoded.value.get("crc64_valid").is_none());
        let header = &decoded.value["header"];
        assert_eq!(header["serial_raw"], SERIAL);
        assert_eq!(header["status_decoding"], "unavailable_version_qualified");
        assert_eq!(
            STANDARD
                .decode(header["status_bytes_base64"].as_str().unwrap())
                .unwrap(),
            HEADER[12..]
        );
        assert!(header.get("alert_flags_raw").is_none());
        assert!(header.get("shot_limiting_level").is_none());
        assert!(decoded.value["columns"][0].get("window_raw").is_none());
        validate_standard_packet(&raw, SERIAL, INIT, 1, Integrity::PreCrcReserved).unwrap();
        assert!(
            validate_standard_packet(&raw, SERIAL, INIT, 1, Integrity::Required)
                .unwrap_err()
                .to_string()
                .contains("CRC64")
        );
        assert!(
            validate_standard_packet(&raw, SERIAL + 1, INIT, 1, Integrity::PreCrcReserved)
                .unwrap_err()
                .to_string()
                .contains("serial")
        );
    }

    #[test]
    fn declared_zone_header_proof_does_not_claim_a_selected_body_profile() {
        // A synthetic minimal STANDARD envelope, not a zone measurement body.
        let mut raw = vec![0; 64];
        let mut header = HEADER;
        header[0] = 3;
        put(&mut raw, 0, &header);
        put(&mut raw, 56, &0x7eb3_6164_c269_0212_u64.to_le_bytes());
        validate_standard_packet(&raw, SERIAL, INIT, 3, Integrity::Required).unwrap();
        validate_standard_packet(&raw, SERIAL, INIT, 2, Integrity::Required).unwrap_err();
        decode(&raw, &format(Profile::ModernSingle)).unwrap_err();
    }

    #[test]
    fn identity_type_and_raw_extent_remain_strict_even_for_other_stream_header_checks() {
        let (configured, raw) = modern_fixture(Profile::ModernSingle);
        let mut wrong = configured;
        wrong.serial = Some(SERIAL + 1);
        assert!(
            decode(&raw, &wrong)
                .unwrap_err()
                .to_string()
                .contains("serial")
        );
        wrong = configured;
        wrong.init_id = Some(INIT + 1);
        let error = crate::types::classify(&decode(&raw, &wrong).unwrap_err());
        assert_eq!(error.kind, ErrorKind::Disconnected);
        let details = error.details.unwrap();
        assert_eq!(details["reason"], "ouster_init_id_changed");
        assert_eq!(details["actual_init_id"], INIT);
        assert!(
            validate_standard_packet(&raw, SERIAL, INIT, 2, Integrity::Required)
                .unwrap_err()
                .to_string()
                .contains("packet type")
        );
        decode(&raw[..raw.len() - 1], &configured).unwrap_err();
        let mut extra = raw.clone();
        extra.push(0);
        decode(&extra, &configured).unwrap_err();
        validate_standard_packet(&[0; 63], SERIAL, INIT, 1, Integrity::Required).unwrap_err();
        validate_standard_packet(&vec![0; 65_508], SERIAL, INIT, 1, Integrity::Required)
            .unwrap_err();
        validate_standard_packet(&raw, SERIAL, INIT, 1, Integrity::NoCrcInFormat).unwrap_err();
    }

    #[test]
    fn known_valid_column_geometry_is_enforced_without_repairing_raw_unknown_states() {
        let (configured, mut raw) = legacy_fixture();
        put(&mut raw, 8, &512_u16.to_le_bytes());
        assert!(
            decode(&raw, &configured)
                .unwrap_err()
                .to_string()
                .contains("measurement ID")
        );
        let (configured, mut raw) = legacy_fixture();
        put(&mut raw, 12, &90_112_u32.to_le_bytes());
        assert!(
            decode(&raw, &configured)
                .unwrap_err()
                .to_string()
                .contains("encoder")
        );
        let (configured, raw) = legacy_fixture();
        let value = decode(&raw, &configured).unwrap().value;
        assert_eq!(value["columns"][2]["measurement_id_in_bounds"], false);
        assert_eq!(value["columns"][2]["encoder_in_bounds"], false);
        assert_eq!(value["columns"][2]["validity"], "unknown");
    }

    #[test]
    fn documented_sizes_bounds_and_conservative_json_admission_are_checked_before_indexing() {
        for (profile, sizes) in [
            (Profile::ModernSingle, [6400, 12544, 24832]),
            (Profile::ModernDual, [8448, 16640, 33024]),
            (Profile::ModernLow, [2304, 4352, 8448]),
            (Profile::ModernLowDual, [4352, 8448, 16640]),
            (Profile::LegacyLidar, [6464, 12608, 24896]),
        ] {
            for (height, size) in [32, 64, 128].into_iter().zip(sizes) {
                let mut configured = format(profile);
                configured.pixels_per_column = height;
                assert_eq!(expected_packet_bytes(&configured).unwrap(), size);
                assert!(decoded_json_bound(&configured).unwrap() < MAX_JSON);
            }
        }
        let mut configured = format(Profile::LegacyLidar);
        configured.pixels_per_column = 16;
        assert_eq!(expected_packet_bytes(&configured).unwrap(), 3392);
        assert_eq!(
            expected_packet_bytes(&format(Profile::LegacyImu)).unwrap(),
            48
        );
        let mut configured = format(Profile::SynchronousImu);
        configured.imu_measurements_per_packet = 1815;
        assert_eq!(expected_packet_bytes(&configured).unwrap(), 65_504);
        assert!(decoded_json_bound(&configured).unwrap() > MAX_JSON);
        configured.imu_measurements_per_packet = 1024;
        assert!(decoded_json_bound(&configured).unwrap() < MAX_JSON);
        for bad in [0, 1816, usize::MAX] {
            configured.imu_measurements_per_packet = bad;
            expected_packet_bytes(&configured).unwrap_err();
        }
        let base = format(Profile::ModernSingle);
        for bad in [
            Format {
                pixels_per_column: usize::MAX,
                ..base
            },
            Format {
                columns_per_packet: usize::MAX,
                ..base
            },
            Format {
                columns_per_frame: 4096,
                ..base
            },
            Format {
                serial: None,
                ..base
            },
            Format {
                serial: Some(1_u64 << 40),
                ..base
            },
            Format {
                init_id: Some(1_u32 << 24),
                ..base
            },
            Format {
                integrity: Integrity::NoCrcInFormat,
                ..base
            },
            Format {
                integrity: Integrity::PreCrcReserved,
                ..base
            },
            Format {
                imu_measurements_per_packet: 1,
                ..base
            },
            Format {
                profile: Profile::ModernLow,
                ..base
            },
        ] {
            expected_packet_bytes(&bad).unwrap_err();
            decode(&[], &bad).unwrap_err();
        }
        let imu = format(Profile::LegacyImu);
        for bad in [
            Format {
                columns_per_frame: 512,
                ..imu
            },
            Format {
                serial: Some(SERIAL),
                ..imu
            },
            Format {
                integrity: Integrity::Required,
                ..imu
            },
            Format {
                header_features: true,
                ..imu
            },
        ] {
            expected_packet_bytes(&bad).unwrap_err();
        }
    }

    #[test]
    fn ieee_object_bound_covers_raw_words_special_states_and_extreme_finite_numbers() {
        for word in [
            0_u32,
            0x8000_0000,
            1,
            0x007f_ffff,
            0x0080_0000,
            0x7f7f_ffff,
            0xff7f_ffff,
            0x7f80_0000,
            0xff80_0000,
            0x7fc1_2345,
            u32::MAX,
        ] {
            let value = ieee(&word.to_le_bytes(), 0).unwrap();
            assert_eq!(value["ieee754_raw"], word);
            assert!(serde_json::to_vec(&value).unwrap().len() <= 96);
            assert!(!value.is_null());
        }
    }
}
