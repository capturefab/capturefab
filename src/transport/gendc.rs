//! Final linear `GenDC` 1.1/1.2 descriptors and byte-exact still companions.
use super::{
    chunks::ByteOrder,
    multipart::{Layout, MAX_BYTES, MAX_PARTS, Part, PartTypeNamespace},
};
use crate::{
    frame::{COORD3D_ABC16, COORD3D_ABC32F, COORD3D_AC16},
    types::{Categorized, ErrorKind, MONO8},
};
use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};

const INTENSITY: u64 = 1;
const RANGE: u64 = 4;
const CONFIDENCE: u64 = 6;
const MONO16: u32 = 0x0110_0007;
const COORD3D_C16: u32 = 0x0110_00b8;
const CONFIDENCE16: u32 = 0x0110_00c7;
const ARRAY: usize = 0x4100;
const IMAGE: usize = 0x4200;
#[cfg(feature = "gentl")]
const COMPONENT_BYTES: usize = 48;
#[cfg(feature = "gentl")]
pub(super) const HEADER_BYTES: usize = 56;
const MAX_DESCRIPTOR: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Selection {
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub(crate) component: u16,
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub(crate) part: u16,
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub(crate) type_id: u64,
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub(crate) header_type: u16,
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub(crate) pfnc_code: u32,
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub(crate) group_id: u16,
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub(crate) source_id: u16,
    #[serde(deserialize_with = "crate::config_number::unsigned")]
    pub(crate) region_id: u16,
}

impl Selection {
    pub(crate) fn validate(self) -> Result<()> {
        ensure!(
            usize::from(self.component) < MAX_PARTS
                && usize::from(self.part) < MAX_PARTS
                && supported(self.type_id, usize::from(self.header_type), self.pfnc_code),
            "invalid GenDC component selection"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartHeader {
    pub component: u16,
    pub part: u16,
    pub descriptor_offset: usize,
    pub header_bytes: usize,
    pub flags: u16,
    pub flow_id: u16,
    pub flow_offset: u64,
    /// Byte padding after each 2D line, or after the whole 1D array.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x_padding_bytes: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub y_padding_bytes: Option<u16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Component {
    pub descriptor_offset: usize,
    pub header_bytes: usize,
    pub invalid: bool,
    pub group_id: u16,
    pub source_id: u16,
    pub region_id: u16,
    pub region_offset_x: u32,
    pub region_offset_y: u32,
    pub timestamp_ns: i64,
    pub type_id: u64,
    pub format: u32,
    pub part_count: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Container {
    pub version: [u8; 3],
    pub id: u64,
    pub flags: u16,
    /// True means the component timestamp epoch is 1970-01-01 00:00:00 TAI.
    pub timestamp_ptp: bool,
    pub header_bytes: usize,
    pub descriptor_bytes: usize,
    pub data_offset: usize,
    pub max_data_bytes: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intended_buffer_bytes: Option<usize>,
    pub components: Vec<Component>,
}

fn unsupported(message: &str) -> anyhow::Error {
    Categorized::error(ErrorKind::Unsupported, message)
}

const fn supported(type_id: u64, header_type: usize, format: u32) -> bool {
    matches!(
        (type_id, header_type, format),
        (INTENSITY, IMAGE, MONO8 | MONO16)
            | (
                RANGE,
                ARRAY | IMAGE,
                COORD3D_C16 | COORD3D_AC16 | COORD3D_ABC16 | COORD3D_ABC32F
            )
            | (CONFIDENCE, ARRAY | IMAGE, CONFIDENCE16)
    )
}

pub(super) struct ImageRows {
    shape: (u32, u32, u32),
    pub row_bytes: usize,
    pub stride: usize,
    pub pixel_bytes: usize,
}

impl Container {
    fn component(&self, part: &Part) -> Option<(&Component, PartHeader)> {
        let header = part.gendc?;
        self.components
            .get(usize::from(header.component))
            .map(|component| (component, header))
    }

    pub(super) fn selection(&self, part: &Part) -> Option<Selection> {
        let (component, header) = self.component(part)?;
        Some(Selection {
            component: header.component,
            part: header.part,
            type_id: component.type_id,
            header_type: u16::try_from(part.data_type).ok()?,
            pfnc_code: u32::try_from(part.format?).ok()?,
            group_id: component.group_id,
            source_id: component.source_id,
            region_id: component.region_id,
        })
    }

    #[cfg(feature = "gentl")]
    pub(super) fn ordinary_intensity(&self, part: &Part) -> bool {
        self.component(part)
            .is_some_and(|(component, _)| component.type_id == INTENSITY)
            && self.image_shape(part).is_ok()
    }

    pub(super) fn image_shape(&self, part: &Part) -> Result<(u32, u32, u32)> {
        Ok(self.image_rows(part)?.shape)
    }

    pub(super) fn image_rows(&self, part: &Part) -> Result<ImageRows> {
        let (component, header) = self.component(part).context("missing GenDC part header")?;
        let format = u32::try_from(part.format.context("missing GenDC part format")?)?;
        ensure!(
            !component.invalid
                && component.part_count == 1
                && component.format == format
                && header.flags == 0
                && supported(component.type_id, part.data_type, format),
            unsupported("selected GenDC component is not a supported image, map or point array")
        );
        let width = u32::try_from(part.width.context("missing GenDC part width")?)?;
        let height = u32::try_from(part.height.context("missing GenDC part height")?)?;
        ensure!(
            width > 0 && height > 0 && (part.data_type != ARRAY || height == 1),
            "invalid GenDC part dimensions"
        );
        let rows = usize::try_from(height)?;
        let row_bytes =
            crate::frame::frame_len(format, width, 1).context("GenDC row size overflow")?;
        // GenDC §2.2.8.2 and §2.2.8.3. The 1D array uses one row, so its
        // end-of-part padding is represented as padding after that row.
        let x_padding = usize::from(
            header
                .x_padding_bytes
                .context("missing GenDC row padding")?,
        );
        let y_padding = usize::from(
            header
                .y_padding_bytes
                .context("missing GenDC part padding")?,
        );
        let stride = row_bytes
            .checked_add(x_padding)
            .context("GenDC stride overflow")?;
        ensure!(
            stride
                .checked_mul(rows)
                .and_then(|span| span.checked_add(y_padding))
                == Some(part.length),
            "GenDC part size differs from its exact padded PFNC geometry"
        );
        let pixel_bytes = row_bytes
            .checked_mul(rows)
            .context("GenDC image size overflow")?;
        Ok(ImageRows {
            shape: (width, height, format),
            row_bytes,
            stride,
            pixel_bytes,
        })
    }

    pub(super) fn validate_ranges(&self, layout: &Layout, bytes: usize) -> Result<()> {
        let announced = layout
            .original_buffer_bytes
            .context("missing GenDC announced buffer size")?;
        ensure!(
            layout.data_type_namespace == Some(PartTypeNamespace::GenDc)
                && layout.byte_order == Some(ByteOrder::Little)
                && layout.original_filled_bytes == bytes
                && bytes <= announced
                && announced <= MAX_BYTES
                && self.header_bytes <= self.descriptor_bytes
                && self.descriptor_bytes <= self.data_offset
                && self.data_offset <= bytes
                && self.descriptor_bytes <= MAX_DESCRIPTOR
                && self
                    .data_offset
                    .checked_add(self.max_data_bytes)
                    .is_some_and(|end| end <= announced && bytes <= end)
                && self.intended_buffer_bytes.is_none_or(|size| size == bytes)
                && (1..=MAX_PARTS).contains(&self.components.len()),
            "invalid GenDC container, descriptor or buffer bounds"
        );
        let mut headers = Vec::with_capacity(1 + self.components.len() + layout.parts.len());
        headers.push((0, self.header_bytes));
        let mut ranges = Vec::with_capacity(layout.parts.len());
        let mut part_start = 0;
        let mut accounted = self.descriptor_bytes;
        for (component_index, component) in self.components.iter().enumerate() {
            headers.push((
                component.descriptor_offset,
                component
                    .descriptor_offset
                    .checked_add(component.header_bytes)
                    .context("GenDC component header range overflow")?,
            ));
            let part_end = part_start + usize::from(component.part_count);
            let parts = layout
                .parts
                .get(part_start..part_end)
                .context("GenDC component part count is inconsistent")?;
            for (index, part) in parts.iter().enumerate() {
                let header = part.gendc.context("missing GenDC part header")?;
                let end = part
                    .stored_offset
                    .checked_add(part.length)
                    .context("GenDC part data range overflow")?;
                ensure!(
                    usize::from(header.component) == component_index
                        && usize::from(header.part) == index
                        && usize::try_from(part.index)? == part_start + index
                        && part.gvsp.is_none()
                        && part.original_offset == Some(part.stored_offset)
                        && part.stored_offset >= self.data_offset
                        && end <= bytes,
                    "invalid GenDC part mapping or data range"
                );
                headers.push((
                    header.descriptor_offset,
                    header
                        .descriptor_offset
                        .checked_add(header.header_bytes)
                        .context("GenDC part header range overflow")?,
                ));
                if part.length > 0 {
                    ranges.push((part.stored_offset, end));
                }
                accounted = accounted
                    .checked_add(part.length)
                    .context("GenDC accounted byte count overflow")?;
            }
            part_start = part_end;
        }
        headers.sort_unstable();
        ranges.sort_unstable();
        ensure!(
            part_start == layout.parts.len()
                && headers
                    .last()
                    .is_some_and(|last| last.1 == self.descriptor_bytes)
                && headers
                    .iter()
                    .zip(headers.iter().skip(1))
                    .all(|(left, right)| left.1 == right.0)
                && ranges
                    .iter()
                    .zip(ranges.iter().skip(1))
                    .all(|(left, right)| left.1 <= right.0)
                && accounted <= bytes,
            "GenDC descriptor headers or data part ranges are inconsistent"
        );
        ensure!(
            accounted == bytes,
            unsupported("GenDC container has bytes outside its descriptor and parts")
        );
        Ok(())
    }
}

#[cfg(feature = "gentl")]
mod parser {
    use super::{
        ARRAY, ByteOrder, COMPONENT_BYTES, Component, Container, HEADER_BYTES, IMAGE, Layout,
        MAX_BYTES, MAX_DESCRIPTOR, MAX_PARTS, Part, PartHeader, PartTypeNamespace, unsupported,
    };
    use anyhow::{Context as _, Result, ensure};

    /// Counts written and intended bytes separately. Neither count alone
    /// describes the locations of bytes written by a producer with gaps.
    #[derive(Clone, Copy)]
    pub(in crate::transport) struct Fill {
        pub(in crate::transport) announced: usize,
        pub(in crate::transport) written: usize,
        pub(in crate::transport) intended: Option<usize>,
    }

    fn read<const N: usize>(data: &[u8], offset: usize) -> Result<[u8; N]> {
        data.get(offset..)
            .and_then(|data| data.first_chunk())
            .copied()
            .context("truncated GenDC descriptor field")
    }

    fn u16_at(data: &[u8], offset: usize) -> Result<u16> {
        Ok(u16::from_le_bytes(read(data, offset)?))
    }

    fn u32_at(data: &[u8], offset: usize) -> Result<u32> {
        Ok(u32::from_le_bytes(read(data, offset)?))
    }

    fn u64_at(data: &[u8], offset: usize) -> Result<u64> {
        Ok(u64::from_le_bytes(read(data, offset)?))
    }

    pub(in crate::transport) fn descriptor_size(bytes: &[u8], fill: Fill) -> Result<usize> {
        ensure!(
            bytes.get(..4) == Some(b"GNDC") && u16_at(bytes, 8)? == 0x1000,
            "invalid GenDC container header"
        );
        ensure!(
            matches!(read::<3>(bytes, 4)?, [1, 1 | 2, _]),
            unsupported("GenDC descriptor version must be 1.1 or 1.2")
        );
        ensure!(
            u16_at(bytes, 24)? == 0,
            unsupported("GenDC capture requires a final descriptor")
        );
        let size = usize::try_from(u32_at(bytes, 48)?)?;
        let data_offset = usize::try_from(u64_at(bytes, 40)?)?;
        let max_data_bytes = usize::try_from(u64_at(bytes, 32)?)?;
        ensure!(
            (HEADER_BYTES..=MAX_DESCRIPTOR).contains(&size)
                && size <= data_offset
                && data_offset <= fill.written
                && data_offset
                    .checked_add(max_data_bytes)
                    .is_some_and(|end| end <= fill.announced && fill.written <= end)
                && fill.written <= fill.announced
                && fill.announced <= MAX_BYTES
                && fill.intended.is_none_or(|size| size == fill.written),
            "GenDC container was not completely written within its announced buffer"
        );
        Ok(size)
    }

    fn header(data: &[u8], offset: usize, min: usize) -> Result<&[u8]> {
        let data = data
            .get(offset..)
            .context("GenDC header offset exceeds descriptor")?;
        let size = usize::try_from(u32_at(data, 4)?)?;
        ensure!(
            size >= min && size.is_multiple_of(8),
            "invalid GenDC header size"
        );
        data.get(..size).context("GenDC header exceeds descriptor")
    }

    fn part(
        index: u32,
        component: u16,
        part_index: u16,
        data: &[u8],
        offset: usize,
    ) -> Result<Part> {
        let head = header(data, offset, 40)?;
        let data_type = usize::from(u16_at(head, 0)?);
        ensure!(
            (0x4000..=0x4fff).contains(&data_type),
            "invalid GenDC part type"
        );
        let flow_id = u16_at(head, 14)?;
        let flow_offset = u64_at(head, 16)?;
        let raw_offset = u64_at(head, 32)?;
        ensure!(
            (flow_id != 0 || flow_offset == raw_offset) && flow_offset <= raw_offset,
            "invalid GenDC part flow offset"
        );
        let stored_offset = usize::try_from(raw_offset)?;
        let (width, height, x_padding_bytes, y_padding_bytes) = match data_type {
            ARRAY => {
                ensure!(head.len() >= 64, "truncated GenDC 1D part header");
                (
                    Some(usize::try_from(u64_at(head, 40)?)?),
                    Some(1),
                    Some(u16_at(head, 48)?),
                    Some(0),
                )
            }
            IMAGE => (
                Some(usize::try_from(u32_at(head, 40)?)?),
                Some(usize::try_from(u32_at(head, 44)?)?),
                Some(u16_at(head, 48)?),
                Some(u16_at(head, 50)?),
            ),
            _ => (None, None, None, None),
        };
        Ok(Part {
            index,
            original_offset: Some(stored_offset),
            stored_offset,
            length: usize::try_from(u64_at(head, 24)?)?,
            data_type,
            format: Some(u64::from(u32_at(head, 8)?)),
            namespace: None,
            width,
            height,
            delivered_height: None,
            x_offset: None,
            y_offset: None,
            x_padding_pixels: None,
            source_id: None,
            region_id: None,
            purpose_id: None,
            gvsp: None,
            gendc: Some(PartHeader {
                component,
                part: part_index,
                descriptor_offset: offset,
                header_bytes: head.len(),
                flags: u16_at(head, 2)?,
                flow_id,
                flow_offset,
                x_padding_bytes,
                y_padding_bytes,
            }),
        })
    }

    /// Interpret only the descriptor. The caller must separately guarantee
    /// that the complete descriptor is readable before providing this slice.
    pub(in crate::transport) fn parse(bytes: &[u8], fill: Fill) -> Result<Layout> {
        let descriptor_bytes = descriptor_size(bytes, fill)?;
        let data = bytes
            .get(..descriptor_bytes)
            .context("truncated GenDC descriptor")?;
        let count = usize::try_from(u32_at(data, 52)?)?;
        let header_bytes = usize::try_from(u32_at(data, 12)?)?;
        ensure!(
            (1..=MAX_PARTS).contains(&count)
                && (HEADER_BYTES + 8 * count..=HEADER_BYTES + 8 * MAX_PARTS)
                    .contains(&header_bytes)
                && header_bytes <= descriptor_bytes
                && header_bytes.is_multiple_of(8),
            "GenDC component table exceeds its bounds"
        );
        let flags = u16_at(data, 10)?;
        ensure!(
            flags & !3 == 0,
            unsupported("unknown GenDC container flags")
        );
        let mut components = Vec::with_capacity(count);
        let mut parts = Vec::new();
        for index in 0..count {
            let offset = usize::try_from(u64_at(data, HEADER_BYTES + 8 * index)?)?;
            let head = header(data, offset, COMPONENT_BYTES)?;
            let component_flags = u16_at(head, 2)?;
            let invalid = component_flags & 1 != 0;
            let part_count = u16_at(head, 46)?;
            let count = usize::from(part_count);
            ensure!(
                u16_at(head, 0)? == 0x2000
                    && component_flags & !1 == 0
                    && (!invalid || flags & 2 != 0)
                    && (count > 0 || invalid)
                    && parts.len() + count <= MAX_PARTS
                    && head.len() == COMPONENT_BYTES + 8 * count,
                "invalid GenDC component header or part table"
            );
            let timestamp_ns = i64::from_le_bytes(read(head, 24)?);
            ensure!(timestamp_ns >= 0, "negative GenDC component timestamp");
            components.push(Component {
                descriptor_offset: offset,
                header_bytes: head.len(),
                invalid,
                group_id: u16_at(head, 10)?,
                source_id: u16_at(head, 12)?,
                region_id: u16_at(head, 14)?,
                region_offset_x: u32_at(head, 16)?,
                region_offset_y: u32_at(head, 20)?,
                timestamp_ns,
                type_id: u64_at(head, 32)?,
                format: u32_at(head, 40)?,
                part_count,
            });
            for part_index in 0..part_count {
                let at =
                    usize::try_from(u64_at(head, COMPONENT_BYTES + 8 * usize::from(part_index))?)?;
                parts.push(part(
                    u32::try_from(parts.len())?,
                    u16::try_from(index)?,
                    part_index,
                    data,
                    at,
                )?);
            }
        }
        let layout = Layout {
            selection_required: false,
            projected_format: None,
            data_type_namespace: Some(PartTypeNamespace::GenDc),
            byte_order: Some(ByteOrder::Little),
            selected: 0,
            original_buffer_bytes: Some(fill.announced),
            original_filled_bytes: fill.written,
            payload_bytes: fill.written,
            parts,
            gendc: Some(Container {
                version: read(data, 4)?,
                id: u64_at(data, 16)?,
                flags,
                timestamp_ptp: flags & 1 != 0,
                header_bytes,
                descriptor_bytes,
                data_offset: usize::try_from(u64_at(data, 40)?)?,
                max_data_bytes: usize::try_from(u64_at(data, 32)?)?,
                intended_buffer_bytes: fill.intended,
                components,
            }),
        };
        layout.validate_ranges(fill.written)?;
        Ok(layout)
    }
}

#[cfg(feature = "gentl")]
pub(super) use parser::{Fill, descriptor_size, parse};
