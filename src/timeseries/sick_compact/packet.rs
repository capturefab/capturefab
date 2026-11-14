//! Pure SICK Compact Type 1 v3/v4, Type 2 v1 and Type 4 v1/v2 decoding.
//!
//! Native words stay packed; no coordinate, pose or clock correction is inferred.
use super::Stream;
use crate::types::{Categorized, ErrorKind};
use anyhow::Result;
use base64::{Engine as _, display::Base64Display, engine::general_purpose::STANDARD};
use serde::{Serialize, Serializer, ser::Error as _};
use serde_json::{Value, json};
use std::{
    fmt,
    io::{self, Write},
    time::Instant,
};

const MAX_PACKET: usize = 65_507;
const MAX_JSON: usize = (1 << 20) - 16 * 1024;
const MAX_CELLS: u64 = 65_536;

fn incomplete(message: &'static str) -> anyhow::Error {
    Categorized::error(ErrorKind::Incomplete, message)
}
fn unsupported(message: &'static str, field: &'static str, value: u32) -> anyhow::Error {
    Categorized::new(ErrorKind::Unsupported, message)
        .details(json!({(field):value}))
        .into()
}
fn remaining(deadline: Instant) -> Result<()> {
    if Instant::now() >= deadline {
        return Err(Categorized::error(
            ErrorKind::Timeout,
            "SICK Compact packet deadline expired",
        ));
    }
    Ok(())
}

pub(super) fn decode(raw: &[u8], stream: Stream, deadline: Instant) -> Result<Value> {
    remaining(deadline)?;
    if !(12..=MAX_PACKET).contains(&raw.len()) {
        return Err(incomplete(
            "SICK Compact datagram has an invalid bounded extent",
        ));
    }
    let (frame, crc) = raw.split_at(raw.len() - 4);
    let stored = u32::from_le_bytes(crc.try_into()?);
    let calculated = crc32fast::hash(frame);
    remaining(deadline)?;
    if calculated != stored {
        return Err(
            Categorized::new(ErrorKind::Incomplete, "SICK Compact CRC32 mismatch")
                .details(
                    json!({"compact_crc32_stored":stored,"compact_crc32_calculated":calculated}),
                )
                .into(),
        );
    }
    if frame.get(..4) != Some(&[2_u8, 2, 2, 2][..]) {
        return Err(incomplete(
            "SICK Compact start marker differs from the documented frame",
        ));
    }
    let mut cursor = Cursor::new(frame);
    cursor.take(4)?;
    let telegram_type = cursor.u32()?;
    let checksum = Checksum {
        algorithm: "crc32_iso_hdlc",
        scope: "frame_without_crc",
        stored,
        calculated,
        status: "verified",
    };
    match (telegram_type, stream) {
        (1, Stream::Points | Stream::Mixed) => {
            render(&points(cursor, checksum, deadline)?, deadline)
        }
        (2, Stream::Imu | Stream::Mixed) => render(&imu(cursor, checksum, deadline)?, deadline),
        (4, Stream::Encoder | Stream::Mixed) => {
            render(&encoder(cursor, checksum, deadline)?, deadline)
        }
        (1 | 2, _) => Err(incomplete(
            "SICK Compact telegram does not match the selected stream",
        )),
        _ => Err(unsupported(
            "SICK Compact telegram type is outside this decoder",
            "compact_telegram_type",
            telegram_type,
        )),
    }
}

struct Cursor<'a> {
    raw: &'a [u8],
    at: usize,
}
impl<'a> Cursor<'a> {
    fn new(raw: &'a [u8]) -> Self {
        Self { raw, at: 0 }
    }
    fn take(&mut self, count: usize) -> Result<&'a [u8]> {
        let end = self
            .at
            .checked_add(count)
            .ok_or_else(|| incomplete("SICK Compact extent arithmetic overflow"))?;
        let value = self.raw.get(self.at..end).ok_or_else(|| {
            incomplete("SICK Compact declared field exceeds its packet/module extent")
        })?;
        self.at = end;
        Ok(value)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into()?))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into()?))
    }
    fn array(&mut self, count: u32, width: usize) -> Result<&'a [u8]> {
        let bytes = usize::try_from(count)?
            .checked_mul(width)
            .ok_or_else(|| incomplete("SICK Compact array extent arithmetic overflow"))?;
        self.take(bytes)
    }
    fn complete(&self) -> Result<()> {
        if self.at != self.raw.len() {
            return Err(incomplete("SICK Compact has undeclared trailing bytes"));
        }
        Ok(())
    }
}
#[derive(Clone, Copy, Serialize)]
struct Checksum {
    algorithm: &'static str,
    scope: &'static str,
    stored: u32,
    calculated: u32,
    status: &'static str,
}
#[derive(Clone, Copy, Serialize)]
struct Timestamp {
    raw: u64,
    unit: &'static str,
    basis: &'static str,
}
fn timestamp(raw: u64) -> Timestamp {
    Timestamp {
        raw,
        unit: "us",
        basis: "unix_microseconds_declared",
    }
}
#[derive(Clone, Copy, Serialize)]
#[serde(untagged)]
enum Shape {
    Line([u32; 1]),
    Beam([u32; 2]),
    Echo([u32; 3]),
}

// Private checked gather geometry. It traverses existing words without a
// temporary native column allocation during serialization/precharge.
#[derive(Clone, Copy)]
struct Gather<'a> {
    raw: &'a [u8],
    bytes: usize,
    width: usize,
    items_per_row: usize,
    item_stride: usize,
    row_stride: usize,
    offset: usize,
}
impl<'a> Gather<'a> {
    fn new(
        raw: &'a [u8],
        rows: usize,
        items: usize,
        width: usize,
        strides: [usize; 3],
    ) -> Result<Self> {
        let [item_stride, row_stride, offset] = strides;
        let bytes = rows
            .checked_mul(items)
            .and_then(|n| n.checked_mul(width))
            .ok_or_else(|| incomplete("SICK Compact column count overflow"))?;
        if bytes > 0 {
            let last = (rows - 1)
                .checked_mul(row_stride)
                .and_then(|n| {
                    (items - 1)
                        .checked_mul(item_stride)
                        .and_then(|v| n.checked_add(v))
                })
                .and_then(|n| n.checked_add(offset))
                .and_then(|n| n.checked_add(width));
            if width == 0 || items == 0 || last.is_none_or(|end| end > raw.len()) {
                return Err(incomplete(
                    "SICK Compact column exceeds its exact measurement extent",
                ));
            }
        }
        Ok(Self {
            raw,
            bytes,
            width,
            items_per_row: items,
            item_stride,
            row_stride,
            offset,
        })
    }
    fn byte(self, index: usize) -> Option<u8> {
        let item = index / self.width;
        let row = item / self.items_per_row;
        let in_row = item % self.items_per_row;
        self.raw
            .get(
                row * self.row_stride
                    + in_row * self.item_stride
                    + self.offset
                    + index % self.width,
            )
            .copied()
    }
}
impl fmt::Display for Gather<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut at = 0;
        while at < self.bytes {
            let count = (self.bytes - at).min(3);
            let mut input = [0_u8; 3];
            for (i, value) in input.iter_mut().take(count).enumerate() {
                *value = self.byte(at + i).ok_or(fmt::Error)?;
            }
            let mut output = [0_u8; 4];
            let written = STANDARD
                .encode_slice(&input[..count], &mut output)
                .map_err(|_| fmt::Error)?;
            formatter.write_str(str::from_utf8(&output[..written]).map_err(|_| fmt::Error)?)?;
            at += count;
        }
        Ok(())
    }
}
#[derive(Clone, Copy)]
struct Encoded<'a> {
    raw: &'a [u8],
    gather: Option<Gather<'a>>,
    deadline: Instant,
}
impl Serialize for Encoded<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        remaining(self.deadline).map_err(S::Error::custom)?;
        let value = if let Some(column) = &self.gather {
            serializer.collect_str(column)
        } else {
            serializer.collect_str(&Base64Display::new(self.raw, &STANDARD))
        }?;
        remaining(self.deadline).map_err(S::Error::custom)?;
        Ok(value)
    }
}
#[derive(Clone, Copy, Serialize)]
struct Packed<'a> {
    encoding: &'static str,
    shape: Shape,
    order: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    unit: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    basis: Option<&'static str>,
    data: Encoded<'a>,
}
fn packed<'a>(
    raw: &'a [u8],
    encoding: &'static str,
    shape: Shape,
    order: &'static str,
    unit: Option<&'static str>,
    deadline: Instant,
) -> Packed<'a> {
    Packed {
        encoding,
        shape,
        order,
        unit,
        basis: None,
        data: Encoded {
            raw,
            gather: None,
            deadline,
        },
    }
}
fn clock_column(raw: &[u8], lines: u32, deadline: Instant) -> Packed<'_> {
    let mut value = packed(
        raw,
        "base64_le_u64",
        Shape::Line([lines]),
        "line",
        Some("us"),
        deadline,
    );
    value.basis = Some("device_timebase_microseconds_declared");
    value
}
#[derive(Clone, Copy, Serialize)]
struct NativeFloat {
    bits: u32,
    classification: &'static str,
    sign_bit: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<f64>,
}
fn float(bits: u32) -> NativeFloat {
    let value = f32::from_bits(bits);
    NativeFloat {
        bits,
        classification: if value.is_nan() {
            "nan"
        } else if value == f32::INFINITY {
            "positive_infinity"
        } else if value == f32::NEG_INFINITY {
            "negative_infinity"
        } else {
            "finite"
        },
        sign_bit: bits >> 31 != 0,
        value: value.is_finite().then_some(f64::from(value)),
    }
}
#[derive(Serialize)]
struct Points<'a> {
    kind: &'static str,
    telegram_type: u32,
    telegram_version: u32,
    byte_order: &'static str,
    checksum: Checksum,
    clock_sync: &'static str,
    telegram_counter: u64,
    transmit_timestamp: Timestamp,
    size_module_0: u32,
    modules: Vec<Module<'a>>,
}
#[derive(Clone, Copy, Serialize)]
struct Module<'a> {
    module_bytes: u32,
    segment_counter: u64,
    frame_number: u64,
    sender_id: u32,
    line_count: u32,
    beams_per_line: u32,
    echoes_per_beam: u32,
    timestamp_start: Packed<'a>,
    timestamp_stop: Packed<'a>,
    phi: Packed<'a>,
    theta_start: Packed<'a>,
    theta_stop: Packed<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    distance_scale: Option<NativeFloat>,
    next_module_size: u32,
    reserved_raw: [u8; 2],
    echo_mask_raw: u8,
    beam_mask_raw: u8,
    beam_line_count: u64,
    echo_cell_count: u64,
    distance_relation: &'static str,
    reflector_mask: u8,
    property_reserved_mask: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    distance: Option<Packed<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rssi: Option<Packed<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    azimuth: Option<Packed<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    properties: Option<Packed<'a>>,
}
fn points(mut cursor: Cursor<'_>, checksum: Checksum, deadline: Instant) -> Result<Points<'_>> {
    let telegram_counter = cursor.u64()?;
    let transmit_timestamp = timestamp(cursor.u64()?);
    let telegram_version = cursor.u32()?;
    if !matches!(telegram_version, 3 | 4) {
        return Err(unsupported(
            "SICK Compact scan version is unsupported",
            "compact_version",
            telegram_version,
        ));
    }
    let size_module_0 = cursor.u32()?;
    let mut result = Points {
        kind: "sick_compact",
        telegram_type: 1,
        telegram_version,
        byte_order: "little_endian",
        checksum,
        clock_sync: "not_verified",
        telegram_counter,
        transmit_timestamp,
        size_module_0,
        modules: Vec::new(),
    };
    let mut charge = Charge::new(deadline);
    count(&result, &mut charge, deadline)?;
    let mut size = size_module_0;
    while size != 0 {
        remaining(deadline)?;
        let raw = cursor.take(usize::try_from(size)?)?;
        let value = module(raw, size, telegram_version, deadline)?;
        if !result.modules.is_empty() {
            let result = charge.write_all(b",");
            remaining(deadline)?;
            result.map_err(|_| incomplete("SICK Compact decoded JSON exceeds its bound"))?;
        }
        count(&value, &mut charge, deadline)?;
        size = value.next_module_size;
        result.modules.push(value);
    }
    cursor.complete()?;
    remaining(deadline)?;
    Ok(result)
}
fn module<'a>(raw: &'a [u8], size: u32, version: u32, deadline: Instant) -> Result<Module<'a>> {
    let mut input = Cursor::new(raw);
    let segment_counter = input.u64()?;
    let frame_number = input.u64()?;
    let sender_id = input.u32()?;
    let lines = input.u32()?;
    let beams = input.u32()?;
    let echoes = input.u32()?;
    let rows = u64::from(beams)
        .checked_mul(u64::from(lines))
        .ok_or_else(|| incomplete("SICK Compact beam count overflows"))?;
    let cells = rows
        .checked_mul(u64::from(echoes))
        .ok_or_else(|| incomplete("SICK Compact echo count overflows"))?;
    if rows > MAX_CELLS || cells > MAX_CELLS {
        return Err(incomplete(
            "SICK Compact geometry exceeds the 65536-cell capture work bound",
        ));
    }
    let timestamp_start = clock_column(input.array(lines, 8)?, lines, deadline);
    let timestamp_stop = clock_column(input.array(lines, 8)?, lines, deadline);
    let shape = Shape::Line([lines]);
    let phi = packed(
        input.array(lines, 4)?,
        "ieee754_le_u32_bits",
        shape,
        "line",
        Some("rad"),
        deadline,
    );
    let theta_start = packed(
        input.array(lines, 4)?,
        "ieee754_le_u32_bits",
        shape,
        "line",
        Some("rad"),
        deadline,
    );
    let theta_stop = packed(
        input.array(lines, 4)?,
        "ieee754_le_u32_bits",
        shape,
        "line",
        Some("rad"),
        deadline,
    );
    let distance_scale = if version == 4 {
        Some(float(input.u32()?))
    } else {
        None
    };
    let next_module_size = input.u32()?;
    let reserved0 = input.u8()?;
    let echo_mask_raw = input.u8()?;
    let beam_mask_raw = input.u8()?;
    let reserved1 = input.u8()?;
    let distance_present = echo_mask_raw & 1 != 0;
    let rssi_present = echo_mask_raw & 2 != 0;
    let property_present = beam_mask_raw & 1 != 0;
    let azimuth_present = beam_mask_raw & 2 != 0;
    let echo_width = 2_usize * usize::from(distance_present) + 2 * usize::from(rssi_present);
    let beam_width = usize::from(property_present) + 2 * usize::from(azimuth_present);
    let echo_bytes = u64::from(echoes)
        .checked_mul(u64::try_from(echo_width)?)
        .ok_or_else(|| incomplete("SICK Compact row byte count overflows"))?;
    let row_bytes = echo_bytes
        .checked_add(u64::try_from(beam_width)?)
        .ok_or_else(|| incomplete("SICK Compact row byte count overflows"))?;
    let data_bytes = row_bytes
        .checked_mul(rows)
        .ok_or_else(|| incomplete("SICK Compact measurement extent overflows"))?;
    let data = input.take(
        usize::try_from(data_bytes)
            .map_err(|_| incomplete("SICK Compact measurement extent is unrepresentable"))?,
    )?;
    input.complete()?;
    let rows_usize = usize::try_from(rows)?;
    // A zero-row shape has no data and needs no representable/iterated stride.
    let stride = if rows == 0 {
        0
    } else {
        usize::try_from(row_bytes)?
    };
    let echo_bytes = if rows == 0 {
        0
    } else {
        usize::try_from(echo_bytes)?
    };
    let echo_shape = Shape::Echo([beams, lines, echoes]);
    let beam_shape = Shape::Beam([beams, lines]);
    let make_column = |items: usize,
                       width: usize,
                       offset: usize,
                       shape: Shape,
                       order,
                       unit|
     -> Result<Packed<'a>> {
        Ok(Packed {
            encoding: if width == 1 {
                "base64_u8"
            } else {
                "base64_le_u16"
            },
            shape,
            order,
            unit,
            basis: None,
            data: Encoded {
                raw: &[],
                gather: Some(Gather::new(
                    data,
                    rows_usize,
                    items,
                    width,
                    [echo_width, stride, offset],
                )?),
                deadline,
            },
        })
    };
    let distance = distance_present
        .then(|| {
            make_column(
                usize::try_from(echoes)?,
                2,
                0,
                echo_shape,
                "beam_line_echo",
                Some("raw"),
            )
        })
        .transpose()?;
    let rssi = rssi_present
        .then(|| {
            make_column(
                usize::try_from(echoes)?,
                2,
                2 * usize::from(distance_present),
                echo_shape,
                "beam_line_echo",
                Some("dimensionless"),
            )
        })
        .transpose()?;
    let azimuth = azimuth_present
        .then(|| {
            make_column(
                1,
                2,
                echo_bytes + usize::from(version == 4 && property_present),
                beam_shape,
                "beam_line",
                Some("raw_theta_uint16"),
            )
        })
        .transpose()?;
    let properties = property_present
        .then(|| {
            make_column(
                1,
                1,
                echo_bytes + 2 * usize::from(version == 3 && azimuth_present),
                beam_shape,
                "beam_line",
                None,
            )
        })
        .transpose()?;
    remaining(deadline)?;
    Ok(Module {
        module_bytes: size,
        segment_counter,
        frame_number,
        sender_id,
        line_count: lines,
        beams_per_line: beams,
        echoes_per_beam: echoes,
        timestamp_start,
        timestamp_stop,
        phi,
        theta_start,
        theta_stop,
        distance_scale,
        next_module_size,
        reserved_raw: [reserved0, reserved1],
        echo_mask_raw,
        beam_mask_raw,
        beam_line_count: rows,
        echo_cell_count: cells,
        distance_relation: if version == 3 {
            "raw_mm"
        } else {
            "raw_times_distance_scale_mm"
        },
        reflector_mask: 1,
        property_reserved_mask: 254,
        distance,
        rssi,
        azimuth,
        properties,
    })
}
#[derive(Clone, Copy, Serialize)]
struct ImuData<'a> {
    acceleration: Packed<'a>,
    angular_velocity: Packed<'a>,
    orientation: Packed<'a>,
}
#[derive(Clone, Copy, Serialize)]
struct Imu<'a> {
    kind: &'static str,
    telegram_type: u32,
    telegram_version: u32,
    byte_order: &'static str,
    checksum: Checksum,
    clock_sync: &'static str,
    timestamp: Timestamp,
    imu: ImuData<'a>,
}
fn imu(mut cursor: Cursor<'_>, checksum: Checksum, deadline: Instant) -> Result<Imu<'_>> {
    if cursor.raw.len() != 60 {
        return Err(incomplete(
            "SICK Compact IMU must be exactly 64 bytes including CRC",
        ));
    }
    let version = cursor.u32()?;
    if version != 1 {
        return Err(unsupported(
            "SICK Compact IMU version is unsupported",
            "compact_version",
            version,
        ));
    }
    let acceleration = packed(
        cursor.take(12)?,
        "ieee754_le_u32_bits",
        Shape::Line([3]),
        "x_y_z",
        Some("m/s^2"),
        deadline,
    );
    let angular_velocity = packed(
        cursor.take(12)?,
        "ieee754_le_u32_bits",
        Shape::Line([3]),
        "x_y_z",
        Some("rad/s"),
        deadline,
    );
    let orientation = packed(
        cursor.take(16)?,
        "ieee754_le_u32_bits",
        Shape::Line([4]),
        "w_x_y_z",
        None,
        deadline,
    );
    let timestamp = timestamp(cursor.u64()?);
    cursor.complete()?;
    Ok(Imu {
        kind: "sick_compact",
        telegram_type: 2,
        telegram_version: version,
        byte_order: "little_endian",
        checksum,
        clock_sync: "not_verified",
        timestamp,
        imu: ImuData {
            acceleration,
            angular_velocity,
            orientation,
        },
    })
}
// Both versions have the same 48-byte body and 88-byte complete datagram.
// The v1 payload length includes sender_id; v2 excludes that header word.
#[derive(Clone, Copy, Serialize)]
struct EncoderData {
    frame_number: u64,
    tick_count: u32,
    tick_count_at_reference_signal_1: u32,
    tick_count_at_reference_signal_2: u32,
    speed: NativeFloat,
    tick_count_timestamp: Timestamp,
    timestamp_of_reference_signal_1: Timestamp,
    timestamp_of_reference_signal_2: Timestamp,
    speed_unit_status: &'static str,
    reference_validity: &'static str,
    input_mode: &'static str,
    angle_conversion: &'static str,
}
#[derive(Clone, Copy, Serialize)]
struct Encoder {
    kind: &'static str,
    telegram_type: u32,
    telegram_version: u32,
    byte_order: &'static str,
    checksum: Checksum,
    clock_sync: &'static str,
    telegram_counter: u64,
    transmit_timestamp: Timestamp,
    payload_length: u32,
    sender_id: u32,
    encoder: EncoderData,
}
fn encoder(mut cursor: Cursor<'_>, checksum: Checksum, deadline: Instant) -> Result<Encoder> {
    remaining(deadline)?;
    if cursor.raw.len() != 84 {
        return Err(incomplete(
            "SICK Compact encoder must be exactly 88 bytes including CRC",
        ));
    }
    let telegram_counter = cursor.u64()?;
    let transmit_timestamp = timestamp(cursor.u64()?);
    let telegram_version = cursor.u32()?;
    let expected_payload = match telegram_version {
        1 => 52,
        2 => 48,
        _ => {
            return Err(unsupported(
                "SICK Compact encoder version is unsupported",
                "compact_version",
                telegram_version,
            ));
        }
    };
    let payload_length = cursor.u32()?;
    if payload_length != expected_payload {
        return Err(incomplete(
            "SICK Compact encoder payload length differs from its versioned header convention",
        ));
    }
    let sender_id = cursor.u32()?;
    let encoder = EncoderData {
        frame_number: cursor.u64()?,
        tick_count: cursor.u32()?,
        tick_count_at_reference_signal_1: cursor.u32()?,
        tick_count_at_reference_signal_2: cursor.u32()?,
        speed: float(cursor.u32()?),
        tick_count_timestamp: timestamp(cursor.u64()?),
        timestamp_of_reference_signal_1: timestamp(cursor.u64()?),
        timestamp_of_reference_signal_2: timestamp(cursor.u64()?),
        speed_unit_status: "configured_conversion_unobserved",
        reference_validity: "not_observed",
        input_mode: "not_observed",
        angle_conversion: "not_inferred",
    };
    cursor.complete()?;
    remaining(deadline)?;
    Ok(Encoder {
        kind: "sick_compact",
        telegram_type: 4,
        telegram_version,
        byte_order: "little_endian",
        checksum,
        clock_sync: "not_verified",
        telegram_counter,
        transmit_timestamp,
        payload_length,
        sender_id,
        encoder,
    })
}
struct Charge {
    bytes: usize,
    deadline: Instant,
}
impl Charge {
    fn new(deadline: Instant) -> Self {
        Self { bytes: 0, deadline }
    }
}
impl Write for Charge {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if Instant::now() >= self.deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Compact serialization deadline",
            ));
        }
        let count = self
            .bytes
            .checked_add(bytes.len())
            .ok_or_else(|| io::Error::other("Compact JSON count overflow"))?;
        if count > MAX_JSON {
            return Err(io::Error::other("Compact JSON bound"));
        }
        self.bytes = count;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
fn count(value: &impl Serialize, charge: &mut Charge, deadline: Instant) -> Result<()> {
    let result = serde_json::to_writer(charge, value);
    remaining(deadline)?;
    result.map_err(|_| {
        incomplete("SICK Compact decoded JSON exceeds its bounded serialization capacity")
    })
}
fn render(value: &impl Serialize, deadline: Instant) -> Result<Value> {
    remaining(deadline)?;
    count(value, &mut Charge::new(deadline), deadline)?;
    let rendered = serde_json::to_value(value);
    remaining(deadline)?;
    let rendered =
        rendered.map_err(|_| incomplete("SICK Compact native value serialization failed"))?;
    count(&rendered, &mut Charge::new(deadline), deadline)?;
    remaining(deadline)?;
    Ok(rendered)
}

#[cfg(test)]
mod tests;
