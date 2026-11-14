//! Bounded SICK Compact TCP framing and lossless native-column parts.
//!
//! No resynchronization, clock conversion, point transform, or packed-RSSI
//! sample/padding interpretation is performed.
use super::{Profile, Stream};
use crate::types::{Categorized, ErrorKind};
use anyhow::Result;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Serialize, Serializer, ser::Error as _};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::{
    fmt,
    io::{self, Write},
    time::Instant,
};

pub(super) const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;
pub(super) const MAX_PARTS: usize = 64;
const MAX_MODULES: usize = 128;
const MAX_COLUMNS: usize = 2048;
const CHUNK_BYTES: usize = 192 * 1024;
const MAX_PART_JSON: usize = 768 * 1024;
const MAX_TOTAL_JSON: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug)]
pub(super) struct Format {
    pub profile: Profile,
    pub stream: Stream,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct FrameNeed {
    pub next: usize,
    pub complete: bool,
}
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
            "SICK Compact TCP packet deadline expired",
        ));
    }
    Ok(())
}
pub(super) fn validate_format(format: Format) -> Result<()> {
    if matches!(
        (format.profile, format.stream),
        (Profile::Lrs4000, Stream::Imu | Stream::Ambient)
    ) {
        return Err(Categorized::error(
            ErrorKind::Unsupported,
            "LRS4000 Compact TCP profile supports only points",
        ));
    }
    Ok(())
}
pub(super) fn minimum_frame_bytes(format: Format) -> usize {
    match (format.profile, format.stream) {
        (Profile::Lrs4000, _) => 36,
        (_, Stream::Points) => 168,
        (_, Stream::Imu) => 88,
        (_, Stream::Ambient) => 86,
    }
}
fn u16_at(raw: &[u8], at: usize) -> Result<u16> {
    Ok(u16::from_le_bytes(
        raw.get(at..at + 2)
            .ok_or_else(|| incomplete("Compact field exceeds its extent"))?
            .try_into()?,
    ))
}
fn u32_at(raw: &[u8], at: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(
        raw.get(at..at + 4)
            .ok_or_else(|| incomplete("Compact field exceeds its extent"))?
            .try_into()?,
    ))
}
fn u64_at(raw: &[u8], at: usize) -> Result<u64> {
    Ok(u64::from_le_bytes(
        raw.get(at..at + 8)
            .ok_or_else(|| incomplete("Compact field exceeds its extent"))?
            .try_into()?,
    ))
}
fn add(a: usize, b: usize) -> Result<usize> {
    a.checked_add(b)
        .ok_or_else(|| incomplete("Compact extent arithmetic overflow"))
}
fn mul(a: usize, b: usize) -> Result<usize> {
    a.checked_mul(b)
        .ok_or_else(|| incomplete("Compact extent arithmetic overflow"))
}
fn target(next: usize, complete: bool) -> Result<FrameNeed> {
    if next > MAX_FRAME_BYTES {
        return Err(incomplete(
            "Compact TCP telegram exceeds the 4MiB software capacity",
        ));
    }
    Ok(FrameNeed { next, complete })
}
fn expected_type(format: Format) -> u32 {
    match (format.profile, format.stream) {
        (Profile::Lrs4000, _) => 1,
        (_, Stream::Points) => 6,
        (_, Stream::Imu) => 7,
        (_, Stream::Ambient) => 3,
    }
}
/// `complete` means the final extent is known; callers must still read through
/// `next`, then validate the whole CRC and native layout before admission.
pub(super) fn frame_need(prefix: &[u8], format: Format, deadline: Instant) -> Result<FrameNeed> {
    remaining(deadline)?;
    validate_format(format)?;
    if prefix.len() < 8 {
        return target(8, false);
    }
    if prefix.get(..4) != Some(&[2_u8; 4][..]) {
        return Err(incomplete(
            "Compact TCP start marker is invalid; resynchronization is disabled",
        ));
    }
    let kind = u32_at(prefix, 4)?;
    if kind != expected_type(format) {
        return Err(unsupported(
            "Compact TCP type differs from the explicit profile/stream",
            "compact_telegram_type",
            kind,
        ));
    }
    if prefix.len() < 32 {
        return target(32, false);
    }
    let version = u32_at(prefix, 24)?;
    if kind != 1 {
        if version != 1 {
            return Err(unsupported(
                "Compact TCP standard-header version is unsupported",
                "compact_version",
                version,
            ));
        }
        let bytes = add(40, usize::try_from(u32_at(prefix, 28)?)?)?;
        if bytes < minimum_frame_bytes(format) {
            return Err(incomplete(
                "Compact TCP payload is shorter than its fixed native header",
            ));
        }
        return target(bytes, true);
    }
    if !matches!(version, 3 | 4) {
        return Err(unsupported(
            "Compact LRS4000 version is unsupported",
            "compact_version",
            version,
        ));
    }
    let mut size = usize::try_from(u32_at(prefix, 28)?)?;
    let mut at = 32;
    let mut modules = 0;
    while size != 0 {
        remaining(deadline)?;
        modules += 1;
        if modules > MAX_MODULES {
            return Err(incomplete(
                "Compact TCP module count exceeds software capacity",
            ));
        }
        let end = add(at, size)?;
        target(add(end, 4)?, false)?;
        if size < 40 + 4 * usize::from(version == 4) {
            return Err(incomplete(
                "Compact module cannot contain its fixed metadata",
            ));
        }
        if prefix.len() < add(at, 32)? {
            return target(add(at, 32)?, false);
        }
        let lines = usize::try_from(u32_at(prefix, add(at, 20)?)?)?;
        let link = add(
            at,
            add(add(32, mul(lines, 28)?)?, 4 * usize::from(version == 4))?,
        )?;
        let need = add(link, 4)?;
        if add(need, 4)? > end {
            return Err(incomplete(
                "Compact module line metadata exceeds its declared extent",
            ));
        }
        if prefix.len() < need {
            return target(need, false);
        }
        size = usize::try_from(u32_at(prefix, link)?)?;
        at = end;
    }
    target(add(at, 4)?, true)
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
struct Clock {
    raw: u64,
    unit: &'static str,
    basis: &'static str,
}
fn clock(raw: u64) -> Clock {
    Clock {
        raw,
        unit: "us",
        basis: "unix_microseconds_declared",
    }
}
fn float(bits: u32) -> Value {
    let value = f32::from_bits(bits);
    let mut result = json!({"bits":bits,"classification":if value.is_nan(){"nan"}else if value==f32::INFINITY{"positive_infinity"}else if value==f32::NEG_INFINITY{"negative_infinity"}else{"finite"},"sign_bit":bits>>31!=0});
    if value.is_finite() {
        result["value"] = json!(f64::from(value));
    }
    result
}
#[derive(Clone, Copy)]
struct Layout {
    base: usize,
    rows: usize,
    items: usize,
    width: usize,
    item_stride: usize,
    row_stride: usize,
}
impl Layout {
    fn bytes(self) -> Result<usize> {
        mul(mul(self.rows, self.items)?, self.width)
    }
    fn check(self, raw: &[u8]) -> Result<()> {
        let bytes = self.bytes()?;
        if bytes != 0 {
            let last = add(
                add(
                    add(self.base, mul(self.rows - 1, self.row_stride)?)?,
                    mul(self.items - 1, self.item_stride)?,
                )?,
                self.width,
            )?;
            if self.width == 0 || last > raw.len() {
                return Err(incomplete(
                    "Compact native column exceeds the checked telegram",
                ));
            }
        }
        Ok(())
    }
    fn at(self, byte: usize) -> usize {
        let item = byte / self.width;
        self.base
            + (item / self.items) * self.row_stride
            + (item % self.items) * self.item_stride
            + byte % self.width
    }
}
#[derive(Clone)]
struct Column {
    id: &'static str,
    module: Option<usize>,
    encoding: &'static str,
    order: &'static str,
    shape: Vec<u64>,
    unit: Option<&'static str>,
    basis: Option<&'static str>,
    layout: Layout,
    opaque_samples: Option<u64>,
}
#[derive(Clone, Copy)]
struct Slice {
    column: usize,
    start: usize,
    bytes: usize,
}
#[derive(Clone, Copy)]
struct Part {
    start: usize,
    end: usize,
}
pub(super) struct Decoded {
    pub parts: usize,
    pub elements: u64,
    pub telegram_type: u32,
    pub json_bytes: Vec<usize>,
    size: usize,
    version: u32,
    counter: u64,
    transmit: Clock,
    checksum: Checksum,
    sha256: String,
    digest: [u8; 32],
    metadata: Value,
    columns: Vec<Column>,
    slices: Vec<Slice>,
    plan: Vec<Part>,
}
struct Contiguous {
    id: &'static str,
    base: usize,
    count: usize,
    width: usize,
    encoding: &'static str,
    order: &'static str,
    shape: Vec<u64>,
    unit: Option<&'static str>,
}
struct Builder {
    columns: Vec<Column>,
    elements: u64,
}
impl Builder {
    fn column(&mut self, column: Column, raw: &[u8]) -> Result<()> {
        column.layout.check(raw)?;
        if self.columns.len() >= MAX_COLUMNS {
            return Err(incomplete(
                "Compact native column count exceeds software capacity",
            ));
        }
        self.columns.push(column);
        Ok(())
    }
    fn contiguous(&mut self, descriptor: Contiguous, raw: &[u8]) -> Result<usize> {
        let Contiguous {
            id,
            base,
            count,
            width,
            encoding,
            order,
            shape,
            unit,
        } = descriptor;
        let end = add(base, mul(count, width)?)?;
        self.column(
            Column {
                id,
                module: None,
                encoding,
                order,
                shape,
                unit,
                basis: None,
                layout: Layout {
                    base,
                    rows: count,
                    items: 1,
                    width,
                    item_stride: width,
                    row_stride: width,
                },
                opaque_samples: None,
            },
            raw,
        )?;
        Ok(end)
    }
}
fn linear(base: usize, count: usize, width: usize) -> Layout {
    Layout {
        base,
        rows: count,
        items: 1,
        width,
        item_stride: width,
        row_stride: width,
    }
}
fn lrs(raw: &[u8], version: u32, builder: &mut Builder, deadline: Instant) -> Result<Value> {
    let first = u32_at(raw, 28)?;
    let mut size = usize::try_from(first)?;
    let mut at = 32;
    let mut modules = Vec::new();
    while size != 0 {
        remaining(deadline)?;
        if modules.len() >= MAX_MODULES {
            return Err(incomplete("Compact LRS module work bound exceeded"));
        }
        let module = modules.len();
        let end = add(at, size)?;
        let data = raw
            .get(at..end)
            .ok_or_else(|| incomplete("Compact module exceeds telegram"))?;
        let lines = usize::try_from(u32_at(data, 20)?)?;
        let beams = usize::try_from(u32_at(data, 24)?)?;
        let echoes = usize::try_from(u32_at(data, 28)?)?;
        let rows = mul(lines, beams)?;
        let cells = mul(rows, echoes)?;
        builder.elements = builder
            .elements
            .checked_add(u64::try_from(cells)?)
            .ok_or_else(|| incomplete("Compact return count overflows"))?;
        let mut pos = add(at, 32)?;
        for (id, width, encoding, unit, basis) in [
            (
                "timestamp_start",
                8,
                "base64_le_u64",
                Some("us"),
                Some("device_timebase_microseconds_declared"),
            ),
            (
                "timestamp_stop",
                8,
                "base64_le_u64",
                Some("us"),
                Some("device_timebase_microseconds_declared"),
            ),
            ("phi", 4, "ieee754_le_u32_bits", Some("rad"), None),
            ("theta_start", 4, "ieee754_le_u32_bits", Some("rad"), None),
            ("theta_stop", 4, "ieee754_le_u32_bits", Some("rad"), None),
        ] {
            builder.column(
                Column {
                    id,
                    module: Some(module),
                    encoding,
                    order: "line",
                    shape: vec![u64::try_from(lines)?],
                    unit,
                    basis,
                    layout: linear(pos, lines, width),
                    opaque_samples: None,
                },
                raw,
            )?;
            pos = add(pos, mul(lines, width)?)?;
        }
        let distance_scale = if version == 4 {
            let value = float(u32_at(raw, pos)?);
            pos = add(pos, 4)?;
            Some(value)
        } else {
            None
        };
        size = usize::try_from(u32_at(raw, pos)?)?;
        pos = add(pos, 4)?;
        let masks: [u8; 4] = raw
            .get(pos..add(pos, 4)?)
            .ok_or_else(|| incomplete("Compact masks exceed module"))?
            .try_into()?;
        let distance = masks[1] & 1 != 0;
        let rssi = masks[1] & 2 != 0;
        let properties = masks[2] & 1 != 0;
        let azimuth = masks[2] & 2 != 0;
        pos = add(pos, 4)?;
        let echo_width = 2 * usize::from(distance) + 2 * usize::from(rssi);
        let echo_bytes = mul(echoes, echo_width)?;
        let stride = add(
            echo_bytes,
            usize::from(properties) + 2 * usize::from(azimuth),
        )?;
        if add(pos, mul(rows, stride)?)? != end {
            return Err(incomplete(
                "Compact LRS module size does not match its complete native layout",
            ));
        }
        for (present, id, width, offset, items, shape, order, unit) in [
            (
                distance,
                "distance",
                2,
                0,
                echoes,
                vec![
                    u64::try_from(beams)?,
                    u64::try_from(lines)?,
                    u64::try_from(echoes)?,
                ],
                "beam_line_echo",
                Some("raw"),
            ),
            (
                rssi,
                "rssi",
                2,
                2 * usize::from(distance),
                echoes,
                vec![
                    u64::try_from(beams)?,
                    u64::try_from(lines)?,
                    u64::try_from(echoes)?,
                ],
                "beam_line_echo",
                Some("dimensionless"),
            ),
            (
                azimuth,
                "azimuth",
                2,
                echo_bytes + usize::from(version == 4 && properties),
                1,
                vec![u64::try_from(beams)?, u64::try_from(lines)?],
                "beam_line",
                Some("raw_theta_uint16"),
            ),
            (
                properties,
                "properties",
                1,
                echo_bytes + 2 * usize::from(version == 3 && azimuth),
                1,
                vec![u64::try_from(beams)?, u64::try_from(lines)?],
                "beam_line",
                None,
            ),
        ] {
            if present {
                builder.column(
                    Column {
                        id,
                        module: Some(module),
                        encoding: if width == 1 {
                            "base64_u8"
                        } else {
                            "base64_le_u16"
                        },
                        order,
                        shape,
                        unit,
                        basis: None,
                        layout: Layout {
                            base: if rows == 0 || items == 0 {
                                pos
                            } else {
                                add(pos, offset)?
                            },
                            rows,
                            items,
                            width,
                            item_stride: echo_width,
                            row_stride: stride,
                        },
                        opaque_samples: None,
                    },
                    raw,
                )?;
            }
        }
        let mut meta = json!({"module_index":module,"module_bytes":end-at,"segment_counter":u64_at(data,0)?,"frame_number":u64_at(data,8)?,"sender_id":u32_at(data,16)?,"line_count":lines,"beams_per_line":beams,"echoes_per_beam":echoes,"next_module_size":size,"reserved_raw":[masks[0],masks[3]],"echo_mask_raw":masks[1],"beam_mask_raw":masks[2],"distance_relation":if version==3{"raw_mm"}else{"raw_times_distance_scale_mm"},"reflector_mask":1,"property_reserved_mask":254});
        if let Some(scale) = distance_scale {
            meta["distance_scale"] = scale;
        }
        modules.push(meta);
        at = end;
    }
    if at != raw.len() {
        return Err(incomplete(
            "Compact LRS frame has undeclared trailing bytes",
        ));
    }
    Ok(json!({"size_module_0":first,"modules":modules}))
}
fn standard(raw: &[u8], kind: u32, builder: &mut Builder, deadline: Instant) -> Result<Value> {
    remaining(deadline)?;
    let payload = usize::try_from(u32_at(raw, 28)?)?;
    if add(36, payload)? != raw.len() {
        return Err(incomplete(
            "Compact payload length differs from complete native extent",
        ));
    }
    let mut meta = json!({"sender_id":u32_at(raw,32)?,"payload_length":payload});
    if kind == 7 {
        if payload != 48 {
            return Err(incomplete("Compact Type7 IMU body must be exactly48 bytes"));
        }
        meta["timestamp"] = serde_json::to_value(clock(u64_at(raw, 36)?))?;
        builder.contiguous(
            Contiguous {
                id: "acceleration",
                base: 44,
                count: 3,
                width: 4,
                encoding: "ieee754_le_u32_bits",
                order: "x_y_z",
                shape: vec![3],
                unit: Some("m/s^2"),
            },
            raw,
        )?;
        builder.contiguous(
            Contiguous {
                id: "angular_velocity",
                base: 56,
                count: 3,
                width: 4,
                encoding: "ieee754_le_u32_bits",
                order: "x_y_z",
                shape: vec![3],
                unit: Some("rad/s"),
            },
            raw,
        )?;
        builder.contiguous(
            Contiguous {
                id: "orientation",
                base: 68,
                count: 4,
                width: 4,
                encoding: "ieee754_le_u32_bits",
                order: "w_x_y_z",
                shape: vec![4],
                unit: None,
            },
            raw,
        )?;
        builder.elements = 1;
        return Ok(meta);
    }
    if kind == 3 {
        if payload < 46 {
            return Err(incomplete("Compact ambient body lacks fixed metadata"));
        }
        let layers = usize::from(u16_at(raw, 60)?);
        let columns = usize::from(u16_at(raw, 62)?);
        let count = mul(layers, columns)?;
        let encoding = u16_at(raw, 80)?;
        if encoding != 0 {
            return Err(unsupported(
                "Compact ambient encoding is unsupported",
                "compact_ambient_encoding",
                u32::from(encoding),
            ));
        }
        if add(82, mul(count, 2)?)? != raw.len() {
            return Err(incomplete(
                "Compact ambient word count differs from payload length",
            ));
        }
        meta["ambient"] = json!({"frame_number":u64_at(raw,36)?,"timestamp_start":clock(u64_at(raw,44)?),"timestamp_stop":clock(u64_at(raw,52)?),"layers":layers,"columns":columns,"encoding_raw":encoding,"horizontal_angle_start":float(u32_at(raw,64)?),"horizontal_angle_stop":float(u32_at(raw,68)?),"vertical_angle_start":float(u32_at(raw,72)?),"vertical_angle_stop":float(u32_at(raw,76)?),"ordering_status":"wire_flat_order_unqualified"});
        builder.contiguous(
            Contiguous {
                id: "ambient_light",
                base: 82,
                count,
                width: 2,
                encoding: "base64_le_u16",
                order: "wire_flat_order_unqualified",
                shape: vec![u64::try_from(count)?],
                unit: None,
            },
            raw,
        )?;
        builder.elements = u64::try_from(count)?;
        return Ok(meta);
    }
    if payload < 128 {
        return Err(incomplete("Compact Type6 body lacks fixed metadata"));
    }
    let header: &[u8; 164] = raw
        .get(..164)
        .ok_or_else(|| incomplete("Compact Type6 fixed header is incomplete"))?
        .try_into()?;
    let columns = usize::from(u16_at(raw, 56)?);
    let layers = usize::from(u16_at(raw, 60)?);
    let echoes = usize::from(header[62]);
    let ambient = usize::from(u16_at(raw, 63)?);
    let cells = mul(mul(columns, layers)?, echoes)?;
    let mask = header[72];
    meta["scan"] = json!({"frame_number":u64_at(raw,36)?,"frame_timestamp":clock(u64_at(raw,44)?),"segment_index":u16_at(raw,52)?,"segments_per_frame":u16_at(raw,54)?,"columns_in_segment":columns,"columns_per_frame":u16_at(raw,58)?,"layers":layers,"echoes":echoes,"ambient_layers":ambient,"interlace_steps":header[65],"interlace_index":header[66],"scan_configuration_identifier":header[67],"distance_scale":float(u32_at(raw,68)?),"data_content_echo_raw":mask,"reserved_payload_base64":STANDARD.encode(&header[73..164]),"distance_relation":"raw_times_distance_scale_mm","column_properties_status":"raw_u16_no_flags_inferred","echo_properties_status":"raw_u8_no_flags_inferred"});
    let mut pos = 164;
    let ambient_count = mul(ambient, columns)?;
    pos = builder.contiguous(
        Contiguous {
            id: "ambient_light",
            base: pos,
            count: ambient_count,
            width: 2,
            encoding: "base64_le_u16",
            order: "wire_flat_order_unqualified",
            shape: vec![u64::try_from(ambient_count)?],
            unit: None,
        },
        raw,
    )?;
    pos = builder.contiguous(
        Contiguous {
            id: "elevation",
            base: pos,
            count: layers,
            width: 4,
            encoding: "ieee754_le_u32_bits",
            order: "layer",
            shape: vec![u64::try_from(layers)?],
            unit: Some("rad"),
        },
        raw,
    )?;
    pos = builder.contiguous(
        Contiguous {
            id: "azimuth",
            base: pos,
            count: columns,
            width: 4,
            encoding: "ieee754_le_u32_bits",
            order: "column",
            shape: vec![u64::try_from(columns)?],
            unit: Some("rad"),
        },
        raw,
    )?;
    let time_column = builder.columns.len();
    pos = builder.contiguous(
        Contiguous {
            id: "relative_timestamp",
            base: pos,
            count: columns,
            width: 4,
            encoding: "base64_le_u32",
            order: "column",
            shape: vec![u64::try_from(columns)?],
            unit: Some("us"),
        },
        raw,
    )?;
    builder.columns[time_column].basis = Some("relative_to_frame_timestamp_declared");
    pos = builder.contiguous(
        Contiguous {
            id: "column_properties",
            base: pos,
            count: columns,
            width: 2,
            encoding: "base64_le_u16",
            order: "column",
            shape: vec![u64::try_from(columns)?],
            unit: None,
        },
        raw,
    )?;
    let shape = vec![
        u64::try_from(columns)?,
        u64::try_from(layers)?,
        u64::try_from(echoes)?,
    ];
    pos = builder.contiguous(
        Contiguous {
            id: "distance",
            base: pos,
            count: cells,
            width: 2,
            encoding: "base64_le_u16",
            order: "column_layer_echo",
            shape: shape.clone(),
            unit: Some("raw"),
        },
        raw,
    )?;
    if mask & 1 != 0 {
        let bytes = mul(cells, 3)? / 2;
        builder.column(
            Column {
                id: "intensity_packed",
                module: None,
                encoding: "opaque_packed_u12",
                order: "wire_packed_bytes",
                shape: vec![u64::try_from(bytes)?],
                unit: None,
                basis: None,
                layout: linear(pos, bytes, 1),
                opaque_samples: Some(u64::try_from(cells)?),
            },
            raw,
        )?;
        pos = add(pos, bytes)?;
    }
    if mask & 2 != 0 {
        pos = builder.contiguous(
            Contiguous {
                id: "pulse_width",
                base: pos,
                count: cells,
                width: 1,
                encoding: "base64_u8",
                order: "wire_flat_order_unqualified",
                shape: vec![u64::try_from(cells)?],
                unit: Some("0.125_ns_raw_units"),
            },
            raw,
        )?;
    }
    if mask & 4 != 0 {
        pos = builder.contiguous(
            Contiguous {
                id: "echo_properties",
                base: pos,
                count: cells,
                width: 1,
                encoding: "base64_u8",
                order: "column_layer_echo",
                shape,
                unit: None,
            },
            raw,
        )?;
    }
    if pos != raw.len() {
        return Err(incomplete(
            "Compact Type6 native geometry differs from payload length",
        ));
    }
    builder.elements = u64::try_from(cells)?;
    Ok(meta)
}

struct Encoded<'a> {
    raw: &'a [u8],
    layout: Layout,
    start: usize,
    bytes: usize,
    deadline: Instant,
}
impl fmt::Display for Encoded<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut at = 0;
        while at < self.bytes {
            // Layout and slice bounds are checked before this formatter. Deadline
            // expiry must be a Serializer error, never fmt::Error: Value's
            // collect_str invokes ToString, which panics on a Display error.
            let count = (self.bytes - at).min(3);
            let mut input = [0_u8; 3];
            for (i, value) in input.iter_mut().take(count).enumerate() {
                *value = *self
                    .raw
                    .get(self.layout.at(self.start + at + i))
                    .ok_or(fmt::Error)?;
            }
            let mut output = [0_u8; 4];
            let written = STANDARD
                .encode_slice(&input[..count], &mut output)
                .map_err(|_| fmt::Error)?;
            f.write_str(str::from_utf8(&output[..written]).map_err(|_| fmt::Error)?)?;
            at += count;
        }
        Ok(())
    }
}
fn serialize_checked<S: Serializer>(
    serializer: S,
    value: &impl fmt::Display,
    mut check: impl FnMut() -> Result<()>,
) -> std::result::Result<S::Ok, S::Error> {
    check().map_err(S::Error::custom)?;
    let result = serializer.collect_str(value);
    check().map_err(S::Error::custom)?;
    result
}
impl Serialize for Encoded<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serialize_checked(serializer, self, || remaining(self.deadline))
    }
}
#[derive(Serialize)]
struct PackedRssi {
    declared_samples: u64,
    declared_sample_bit_count: u64,
    available_bit_count: u64,
    global_bit_range: [u64; 2],
    interpretation: &'static str,
}
#[derive(Serialize)]
struct SourceLayout {
    base_byte_offset: usize,
    rows: usize,
    items_per_row: usize,
    item_width: usize,
    item_stride: usize,
    row_stride: usize,
}
#[derive(Serialize)]
struct ColumnView<'a> {
    id: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    module_index: Option<usize>,
    encoding: &'static str,
    order: &'static str,
    shape: &'a [u64],
    total_elements: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    global_element_range: Option<[u64; 2]>,
    global_byte_range: [u64; 2],
    source_layout: SourceLayout,
    #[serde(skip_serializing_if = "Option::is_none")]
    unit: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    basis: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    packed_rssi: Option<PackedRssi>,
    data: Encoded<'a>,
}
fn column_view<'a>(
    raw: &'a [u8],
    column: &'a Column,
    slice: Slice,
    deadline: Instant,
) -> Result<ColumnView<'a>> {
    let end = add(slice.start, slice.bytes)?;
    let layout = column.layout;
    let total = layout.bytes()?;
    let opaque = column.opaque_samples;
    Ok(ColumnView {
        id: column.id,
        module_index: column.module,
        encoding: column.encoding,
        order: column.order,
        shape: &column.shape,
        total_elements: u64::try_from(total / layout.width)?,
        global_element_range: if opaque.is_some() {
            None
        } else {
            Some([
                u64::try_from(slice.start / layout.width)?,
                u64::try_from(end / layout.width)?,
            ])
        },
        global_byte_range: [u64::try_from(slice.start)?, u64::try_from(end)?],
        source_layout: SourceLayout {
            base_byte_offset: layout.base,
            rows: layout.rows,
            items_per_row: layout.items,
            item_width: layout.width,
            item_stride: layout.item_stride,
            row_stride: layout.row_stride,
        },
        unit: column.unit,
        basis: column.basis,
        packed_rssi: opaque.map(|samples| PackedRssi {
            declared_samples: samples,
            declared_sample_bit_count: samples * 12,
            available_bit_count: total as u64 * 8,
            global_bit_range: [slice.start as u64 * 8, end as u64 * 8],
            interpretation: "unqualified_no_values_or_padding_inferred",
        }),
        data: Encoded {
            raw,
            layout,
            start: slice.start,
            bytes: slice.bytes,
            deadline,
        },
    })
}
#[derive(Serialize)]
struct NativePart<'a> {
    kind: &'static str,
    telegram_type: u32,
    telegram_version: u32,
    byte_order: &'static str,
    telegram_counter: u64,
    transmit_timestamp: Clock,
    checksum: Checksum,
    clock_sync: &'static str,
    telegram_sha256: &'a str,
    native_elements_declared: u64,
    native_elements_scope: &'static str,
    part_index: usize,
    part_count: usize,
    native_assembly: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    native_metadata: Option<&'a Value>,
    native_columns: Vec<ColumnView<'a>>,
}
impl Decoded {
    fn view<'a>(
        &'a self,
        raw: &'a [u8],
        part: Part,
        index: usize,
        count: usize,
        deadline: Instant,
    ) -> Result<NativePart<'a>> {
        let mut columns = Vec::with_capacity(part.end - part.start);
        for slice in &self.slices[part.start..part.end] {
            columns.push(column_view(
                raw,
                &self.columns[slice.column],
                *slice,
                deadline,
            )?);
        }
        Ok(NativePart {
            kind: "sick_compact_tcp",
            telegram_type: self.telegram_type,
            telegram_version: self.version,
            byte_order: "little_endian",
            telegram_counter: self.counter,
            transmit_timestamp: self.transmit,
            checksum: self.checksum,
            clock_sync: "not_verified",
            telegram_sha256: &self.sha256,
            native_elements_declared: self.elements,
            native_elements_scope: "complete_telegram_not_per_part",
            part_index: index,
            part_count: count,
            native_assembly: "all_parts_share_one_verified_telegram",
            native_metadata: (index == 0).then_some(&self.metadata),
            native_columns: columns,
        })
    }
}
struct Charge {
    bytes: usize,
    limit: usize,
    deadline: Instant,
}
impl Write for Charge {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if Instant::now() >= self.deadline {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "Compact deadline"));
        }
        let count = self
            .bytes
            .checked_add(bytes.len())
            .ok_or_else(|| io::Error::other("Compact size overflow"))?;
        if count > self.limit {
            return Err(io::Error::other("Compact JSON bound"));
        }
        self.bytes = count;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
fn size(value: &impl Serialize, limit: usize, deadline: Instant) -> Result<usize> {
    let mut charge = Charge {
        bytes: 0,
        limit,
        deadline,
    };
    let result = serde_json::to_writer(&mut charge, value);
    remaining(deadline)?;
    result
        .map_err(|_| incomplete("Compact native JSON exceeds bounded materialization capacity"))?;
    Ok(charge.bytes)
}
pub(super) fn validate(raw: &[u8], format: Format, deadline: Instant) -> Result<Decoded> {
    remaining(deadline)?;
    let need = frame_need(raw, format, deadline)?;
    if !need.complete || raw.len() != need.next || !(12..=MAX_FRAME_BYTES).contains(&raw.len()) {
        return Err(incomplete(
            "Compact TCP frame is incomplete or has trailing bytes",
        ));
    }
    let end = raw.len() - 4;
    let frame = &raw[..end];
    let stored = u32_at(raw, end)?;
    let calculated = crc32fast::hash(frame);
    remaining(deadline)?;
    if stored != calculated {
        return Err(Categorized::new(
            ErrorKind::Incomplete,
            "Compact TCP whole-frame CRC32 mismatch",
        )
        .details(json!({"compact_crc32_stored":stored,"compact_crc32_calculated":calculated}))
        .into());
    }
    let kind = u32_at(frame, 4)?;
    let version = u32_at(frame, 24)?;
    let mut builder = Builder {
        columns: Vec::new(),
        elements: 0,
    };
    let metadata = if kind == 1 {
        lrs(frame, version, &mut builder, deadline)?
    } else {
        standard(frame, kind, &mut builder, deadline)?
    };
    let digest: [u8; 32] = Sha256::digest(raw).into();
    remaining(deadline)?;
    let mut value = Decoded {
        parts: 0,
        elements: builder.elements,
        telegram_type: kind,
        json_bytes: Vec::new(),
        size: raw.len(),
        version,
        counter: u64_at(frame, 8)?,
        transmit: clock(u64_at(frame, 16)?),
        checksum: Checksum {
            algorithm: "crc32_iso_hdlc",
            scope: "frame_without_crc",
            stored,
            calculated,
            status: "verified",
        },
        sha256: crate::types::hex(digest),
        digest,
        metadata,
        columns: builder.columns,
        slices: Vec::new(),
        plan: Vec::new(),
    };
    for (index, column) in value.columns.iter().enumerate() {
        let bytes = column.layout.bytes()?;
        let chunk = CHUNK_BYTES / column.layout.width * column.layout.width;
        let mut at = 0;
        loop {
            if value.slices.len() >= MAX_COLUMNS {
                return Err(incomplete(
                    "Compact native chunk count exceeds software capacity",
                ));
            }
            let count = (bytes - at).min(chunk);
            value.slices.push(Slice {
                column: index,
                start: at,
                bytes: count,
            });
            at = add(at, count)?;
            if at == bytes {
                break;
            }
        }
    }
    // Charge each borrowed native slice once. Conservative two-digit part fields
    // allow exact grouping without repeatedly hashing/materializing payloads.
    let mut lengths = Vec::with_capacity(value.slices.len());
    for slice in &value.slices {
        remaining(deadline)?;
        lengths.push(size(
            &column_view(raw, &value.columns[slice.column], *slice, deadline)?,
            MAX_PART_JSON,
            deadline,
        )?);
    }
    let mut first = 0;
    let mut index = 0;
    loop {
        let mut count = size(
            &value.view(
                raw,
                Part {
                    start: first,
                    end: first,
                },
                if index == 0 { 0 } else { 63 },
                64,
                deadline,
            )?,
            MAX_PART_JSON,
            deadline,
        )?;
        let mut last = first;
        while last < lengths.len() {
            let next = add(count, add(lengths[last], usize::from(last != first))?)?;
            if next > MAX_PART_JSON {
                break;
            }
            count = next;
            last += 1;
        }
        if last == first && first < lengths.len() {
            return Err(incomplete(
                "Compact native column cannot fit one bounded part",
            ));
        }
        value.plan.push(Part {
            start: first,
            end: last,
        });
        if value.plan.len() > MAX_PARTS {
            return Err(incomplete(
                "Compact native part count exceeds64 software capacity",
            ));
        }
        if last == lengths.len() {
            break;
        }
        first = last;
        index += 1;
    }
    value.parts = value.plan.len();
    let mut total = 0;
    for (index, part) in value.plan.iter().enumerate() {
        let bytes = size(
            &value.view(raw, *part, index, value.parts, deadline)?,
            MAX_PART_JSON,
            deadline,
        )?;
        total = add(total, bytes)?;
        if total > MAX_TOTAL_JSON {
            return Err(incomplete(
                "Compact complete native JSON exceeds16MiB software capacity",
            ));
        }
        value.json_bytes.push(bytes);
    }
    remaining(deadline)?;
    Ok(value)
}
pub(super) fn materialize(
    decoded: &Decoded,
    raw: &[u8],
    part: usize,
    deadline: Instant,
) -> Result<Value> {
    remaining(deadline)?;
    if raw.len() != decoded.size {
        return Err(incomplete("Compact native plan raw extent differs"));
    }
    let digest: [u8; 32] = Sha256::digest(raw).into();
    remaining(deadline)?;
    if digest != decoded.digest {
        return Err(incomplete(
            "Compact native plan does not match its immutable raw telegram",
        ));
    }
    remaining(deadline)?;
    let span = *decoded
        .plan
        .get(part)
        .ok_or_else(|| incomplete("Compact native part index is outside its checked plan"))?;
    let view = decoded.view(raw, span, part, decoded.parts, deadline)?;
    let rendered = serde_json::to_value(view);
    remaining(deadline)?;
    let rendered =
        rendered.map_err(|_| incomplete("Compact native JSON materialization failed"))?;
    if size(&rendered, MAX_PART_JSON, deadline)? != decoded.json_bytes[part] {
        return Err(incomplete(
            "Compact native part differs from its admitted exact JSON size",
        ));
    }
    remaining(deadline)?;
    Ok(rendered)
}

#[cfg(test)]
#[path = "packet_tests.rs"]
mod tests;
