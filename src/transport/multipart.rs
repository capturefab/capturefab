//! Lossless archive of validated multipart component bytes.
use super::{chunks::ByteOrder, gendc};
use crate::framebuf::{PoolVec, pool};
use crate::{
    framebuf::FrameBuf,
    types::{Categorized, ErrorKind},
};
use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};

pub(crate) const MAX_PARTS: usize = 32;
pub(crate) const MAX_BYTES: usize = 128 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PartTypeNamespace {
    Gvsp,
    #[serde(rename = "gendc")]
    GenDc,
}

/// Fields carried by the native multipart descriptor. Padding and opaque
/// words are recorded as reported in the wire table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GvspPart {
    /// Byte address in the complete native GVSP payload block.
    pub address_offset: usize,
    /// The leader's maximum part length when the trailer delivered a prefix.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub declared_length: Option<usize>,
    pub zone_directions: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x_padding: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub y_padding: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub type_specific_leader: Option<[u32; 6]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub type_specific_trailer: Option<[u32; 2]>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Part {
    pub index: u32,
    /// Byte offset in the producer's announced buffer, when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_offset: Option<usize>,
    pub stored_offset: usize,
    pub length: usize,
    pub data_type: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivered_height: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x_offset: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub y_offset: Option<usize>,
    /// `GenTL` part `XPadding` is pixels, unlike ordinary `IMAGE` byte padding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x_padding_pixels: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purpose_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gvsp: Option<GvspPart>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gendc: Option<gendc::PartHeader>,
}

impl Part {
    pub(crate) fn has_gendc_padding(&self) -> bool {
        self.gendc.is_some_and(|header| {
            header.x_padding_bytes.unwrap_or(0) != 0 || header.y_padding_bytes.unwrap_or(0) != 0
        })
    }

    fn image_type_supported(&self) -> bool {
        if matches!(
            self.measurement_format(),
            Some(
                crate::frame::COORD3D_C10P
                    | crate::frame::COORD3D_C12P
                    | crate::frame::COORD3D_AC16
                    | crate::frame::COORD3D_ABC16
                    | crate::frame::COORD3D_ABC32F
            )
        ) {
            return self.data_type == 5 && self.namespace == Some(4);
        }
        self.data_type == 1
            || (self.namespace == Some(4)
                && matches!(
                    (self.data_type, self.format),
                    (5, Some(0x0110_00b8)) | (9, Some(0x0108_00c6 | 0x0110_00c7))
                ))
    }

    pub(crate) fn measurement_format(&self) -> Option<u32> {
        let format = u32::try_from(self.format?).ok()?;
        crate::frame::is_measurement_format(format).then_some(format)
    }

    pub(crate) fn image_shape(&self, byte_order: Option<ByteOrder>) -> Result<(u32, u32, u32)> {
        let unsupported = |message| Categorized::error(ErrorKind::Unsupported, message);
        ensure!(
            self.image_type_supported() && self.length > 0,
            unsupported("the selected multipart part is not a supported image or scalar map")
        );
        ensure!(
            self.x_padding_pixels == Some(0),
            unsupported(
                "selected multipart image padding requires an explicit pixel-packing layout"
            )
        );
        ensure!(
            matches!(self.namespace, Some(1 | 4)),
            unsupported("multipart image requires a GEV or 32-bit PFNC namespace")
        );
        let pixel_format =
            u32::try_from(self.format.context("multipart image format is missing")?)?;
        ensure!(
            pixel_format & 0x8000_0000 == 0 && (pixel_format >> 16) & 0xff > 0,
            unsupported("multipart image pixel format requires a known standard mapping")
        );
        let width = u32::try_from(self.width.context("multipart image width is missing")?)?;
        let height = u32::try_from(self.height.context("multipart image height is missing")?)?;
        let delivered = u32::try_from(self.delivered_height.unwrap_or(usize::try_from(height)?))?;
        if let Some(format) = crate::frame::bicolor::Format::from_code(pixel_format) {
            format.validate_width(width)?;
        }
        if let Some(group) = crate::frame::packed_coordinate_group(pixel_format) {
            ensure!(
                width.is_multiple_of(group),
                unsupported("GenTL packed coordinate parts require byte-aligned rows")
            );
            ensure!(
                byte_order == Some(ByteOrder::Little),
                unsupported(
                    "GenTL packed coordinate parts require confirmed little-endian packing"
                )
            );
        }
        ensure!(
            width > 0 && delivered > 0 && delivered <= height,
            "invalid multipart image geometry"
        );
        ensure!(
            crate::frame::frame_len(pixel_format, width, delivered) == Some(self.length),
            "multipart image bytes differ from its exact geometry"
        );
        ensure!(
            word_bytes(pixel_format).is_none() || byte_order.is_some(),
            unsupported("multipart wide-sample byte order is unavailable")
        );
        ensure!(
            byte_order == Some(ByteOrder::Little)
                || (pixel_format >> 16) & 0xff <= 8
                || crate::frame::pixel_format_name(pixel_format) != "Unknown",
            unsupported("unknown multipart multi-byte packing cannot be normalized")
        );
        Ok((width, delivered, pixel_format))
    }
}

/// The PFNC packing unit whose bytes carry the reported pixel endianness.
/// PFNC2.4 figures6-7/6-8 pack RGB10p32/RGB565p into32/16-bit words.
pub(super) const fn word_bytes(pixel_format: u32) -> Option<usize> {
    match pixel_format {
        0x0210_0035 | 0x0210_0036 => Some(2),
        0x0220_001d => Some(4),
        _ => super::gige::sample_bytes(pixel_format),
    }
}
/// A finite exact part format selected for a native GVSP or `GenTL` multipart still.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
pub enum GvspPartFormat {
    #[serde(rename = "Mono8")]
    #[value(name = "Mono8")]
    Mono8,
    #[serde(rename = "RGB8")]
    #[value(name = "RGB8")]
    Rgb8,
    #[serde(rename = "YCbCr411_8")]
    #[value(name = "YCbCr411_8")]
    Ycbcr4118,
    #[serde(rename = "Coord3D_C16")]
    #[value(name = "Coord3D_C16")]
    Coord3dC16,
    #[serde(rename = "Coord3D_C10p")]
    #[value(name = "Coord3D_C10p")]
    Coord3dC10p,
    #[serde(rename = "Coord3D_C12p")]
    #[value(name = "Coord3D_C12p")]
    Coord3dC12p,
    #[serde(rename = "Confidence8")]
    #[value(name = "Confidence8")]
    Confidence8,
    #[serde(rename = "Confidence16")]
    #[value(name = "Confidence16")]
    Confidence16,
    #[serde(rename = "Mono16")]
    #[value(name = "Mono16")]
    Mono16,
    #[serde(rename = "Coord3D_ABC16")]
    #[value(name = "Coord3D_ABC16")]
    Coord3dAbc16,
    #[serde(rename = "Coord3D_ABC32f")]
    #[value(name = "Coord3D_ABC32f")]
    Coord3dAbc32f,
}

impl GvspPartFormat {
    const ALL: [Self; 11] = [
        Self::Mono8,
        Self::Rgb8,
        Self::Ycbcr4118,
        Self::Coord3dC16,
        Self::Coord3dC10p,
        Self::Coord3dC12p,
        Self::Confidence8,
        Self::Confidence16,
        Self::Mono16,
        Self::Coord3dAbc16,
        Self::Coord3dAbc32f,
    ];

    const fn wire(self) -> (usize, u32) {
        match self {
            Self::Mono8 => (1, crate::types::MONO8),
            Self::Rgb8 => (1, crate::types::RGB8),
            Self::Ycbcr4118 => (1, 0x020c_005a),
            Self::Coord3dC16 => (5, 0x0110_00b8),
            Self::Coord3dC10p => (5, crate::frame::COORD3D_C10P),
            Self::Coord3dC12p => (5, crate::frame::COORD3D_C12P),
            Self::Confidence8 => (9, 0x0108_00c6),
            Self::Confidence16 => (9, 0x0110_00c7),
            Self::Mono16 => (1, 0x0110_0007),
            Self::Coord3dAbc16 => (5, crate::frame::COORD3D_ABC16),
            Self::Coord3dAbc32f => (5, crate::frame::COORD3D_ABC32F),
        }
    }

    fn matches(self, part: &Part) -> bool {
        let (data_type, pfnc) = self.wire();
        part.data_type == data_type && part.format == Some(u64::from(pfnc))
    }

    pub(super) fn for_part(part: &Part) -> Option<Self> {
        Self::ALL.into_iter().find(|format| format.matches(part))
    }

    pub(crate) fn validate_output(self, output: &str, format: &str) -> Result<()> {
        if matches!(self, Self::Coord3dAbc16 | Self::Coord3dAbc32f) {
            ensure!(
                output != "-" && format == "raw",
                "--part-format {} requires a local RAW still",
                crate::frame::pixel_format_name(self.wire().1)
            );
        } else {
            ensure!(
                output != "-" && matches!(format, "raw" | "png"),
                "--part-format requires a local RAW or PNG still"
            );
        }
        Ok(())
    }

    pub(crate) const fn ordinary(self) -> bool {
        matches!(self, Self::Mono8 | Self::Rgb8 | Self::Ycbcr4118)
    }

    pub(crate) const fn measurement(self) -> bool {
        crate::frame::is_measurement_format(self.wire().1) || matches!(self, Self::Confidence8)
    }

    pub(crate) fn validate_index(index: Option<u32>, format: Option<Self>) -> Result<()> {
        ensure!(
            index.is_none_or(|index| index < 32),
            "--part-index must be 0..31"
        );
        ensure!(
            index.is_none() || format.is_some(),
            "--part-index requires --part-format"
        );
        Ok(())
    }

    /// Per-job ordinals never override an explicit binding-level selection.
    pub(crate) fn validate_binding(self, index: Option<u32>, camera: &str) -> Result<()> {
        Self::validate_index(index, Some(self))?;
        if camera.starts_with("gentl:") {
            let binding = super::gentl_selector::Selector::parse(camera)?;
            ensure!(
                binding.gendc.is_none(),
                "per-job multipart selection cannot override a GenDC binding"
            );
            if let Some(selected) = binding.part {
                ensure!(
                    index == Some(selected),
                    "--part-index must match the explicit GenTL camera part; use the base camera for different per-job parts"
                );
                ensure!(
                    binding.part_format == (!self.ordinary()).then_some(self.wire().1),
                    "--part-format conflicts with the explicit GenTL camera format or ordinary-image binding"
                );
            }
        }
        Ok(())
    }

    pub(crate) fn validate_transport(transport: crate::types::Transport) -> Result<()> {
        ensure!(
            matches!(
                transport,
                crate::types::Transport::GigE | crate::types::Transport::GenTl
            ),
            Categorized::error(
                ErrorKind::Unsupported,
                "--part-format requires native GigE Vision or GenTL multipart"
            )
        );
        Ok(())
    }

    pub(super) fn shape(self, part: &Part, order: Option<ByteOrder>) -> Result<(u32, u32, u32)> {
        let maximum = part.height.context("missing GVSP declared part height")?;
        let height = part
            .delivered_height
            .context("missing GVSP delivered part height")?;
        ensure!(
            height > 0 && height <= maximum,
            "GVSP selected part trailer height disagrees with its leader"
        );
        let mut actual = *part;
        actual.height = Some(height);
        self.declared_shape(&actual, order)
    }

    pub(super) fn declared_shape(
        self,
        part: &Part,
        order: Option<ByteOrder>,
    ) -> Result<(u32, u32, u32)> {
        let unsupported = |message| Categorized::error(ErrorKind::Unsupported, message);
        let gvsp = part
            .gvsp
            .context("selected part is missing its GVSP descriptor")?;
        ensure!(
            part.namespace.is_none()
                && gvsp.x_padding == Some(0)
                && gvsp.y_padding == Some(0)
                && gvsp.zone_directions & 0x8000_0000 == 0,
            unsupported("GVSP selected parts require one top-down zone and unpadded samples")
        );
        let (_, pixel_format) = self.wire();
        ensure!(
            word_bytes(pixel_format).is_none() || order.is_some(),
            unsupported("GVSP selected part sample byte order is unavailable")
        );
        let width = u32::try_from(part.width.context("missing GVSP selected part width")?)?;
        let height = u32::try_from(part.height.context("missing GVSP selected part height")?)?;
        if let Some(group) = crate::frame::packed_coordinate_group(pixel_format) {
            ensure!(
                width.is_multiple_of(group),
                unsupported("GVSP packed coordinate parts require byte-aligned rows")
            );
            ensure!(
                order == Some(ByteOrder::Little),
                unsupported("GVSP packed coordinate parts require confirmed little-endian packing")
            );
        }
        ensure!(
            width > 0
                && height > 0
                && crate::frame::frame_len(pixel_format, width, height) == Some(part.length),
            "GVSP selected part size disagrees with its exact geometry"
        );
        Ok((width, height, pixel_format))
    }

    fn resolve(
        self,
        frame: &crate::types::Frame,
        index: Option<u32>,
    ) -> Result<(&MultipartData, &Part, (u32, u32, u32))> {
        Self::validate_index(index, Some(self))?;
        ensure!(
            frame.pixel_encoding.is_none(),
            "vendor encoding is not a multipart selection"
        );
        let unsupported = |message| Categorized::error(ErrorKind::Unsupported, message);
        let archive = frame
            .multipart
            .as_ref()
            .filter(|data| {
                data.layout.gendc.is_none()
                    && matches!(
                        data.layout.data_type_namespace,
                        None | Some(PartTypeNamespace::Gvsp)
                    )
            })
            .ok_or_else(|| {
                unsupported("--part-format requires a native GVSP or GenTL multipart frame")
            })?;
        let name = crate::frame::pixel_format_name(self.wire().1);
        let mut matching = archive
            .layout
            .parts
            .iter()
            .filter(|part| index.is_none_or(|index| part.index == index) && self.matches(part));
        let part = matching.next().ok_or_else(|| {
            Categorized::error(
                ErrorKind::Unsupported,
                format!("multipart frame has no {name} part with its required wire type at the requested index"),
            )
        })?;
        ensure!(
            matching.next().is_none(),
            Categorized::error(
                ErrorKind::Unsupported,
                format!("multipart has multiple {name} parts; pass --part-index")
            )
        );
        let (width, height, pixel_format) =
            if archive.layout.data_type_namespace == Some(PartTypeNamespace::Gvsp) {
                self.shape(part, archive.layout.byte_order)?
            } else {
                part.image_shape(archive.layout.byte_order)?
            };
        ensure!(
            self != Self::Ycbcr4118 || width.is_multiple_of(4),
            "YCbCr411_8 metering requires complete four-pixel rows"
        );
        Ok((archive, part, (width, height, pixel_format)))
    }

    /// Borrow the validated ordinary source bytes; metering never allocates
    /// a selected image or clones its part table before saver admission.
    pub(crate) fn ordinary_view(
        self,
        frame: &crate::types::Frame,
        index: Option<u32>,
    ) -> Result<OrdinaryPart<'_>> {
        ensure!(
            self.ordinary(),
            Categorized::error(
                ErrorKind::Unsupported,
                "automatic image tuning requires an ordinary Mono8, RGB8 or YCbCr411_8 part"
            )
        );
        let (archive, part, (width, height, _)) = self.resolve(frame, index)?;
        let end = part
            .stored_offset
            .checked_add(part.length)
            .context("multipart image range overflow")?;
        let data = archive
            .payload
            .get(part.stored_offset..end)
            .context("multipart image lies outside its archive")?;
        Ok(OrdinaryPart {
            data,
            width,
            height,
            format: self,
        })
    }

    pub(crate) fn project(self, frame: &crate::types::Frame) -> Result<crate::types::Frame> {
        self.project_index(frame, None)
    }

    pub(crate) fn project_index(
        self,
        frame: &crate::types::Frame,
        index: Option<u32>,
    ) -> Result<crate::types::Frame> {
        let (archive, part, (width, height, pixel_format)) = self.resolve(frame, index)?;
        if archive.layout.selected == part.index {
            ensure!(
                (frame.width, frame.height, frame.pixel_format) == (width, height, pixel_format)
                    && frame.data.len() == part.length,
                "GVSP representative pixels disagree with the selected part"
            );
            let mut selected = frame.clone();
            selected.jpeg = None;
            let mut layout = archive.layout.clone();
            layout.selection_required = false;
            layout.projected_format = Some(self);
            selected.multipart = Some(
                MultipartData {
                    layout,
                    payload: archive.payload.clone(),
                }
                .into(),
            );
            return Ok(selected);
        }
        let end = part
            .stored_offset
            .checked_add(part.length)
            .context("GVSP map range overflow")?;
        let source = archive
            .payload
            .get(part.stored_offset..end)
            .context("GVSP map lies outside its retained archive")?;
        // One output copy. Wide samples are normalized only in the selected pixels.
        crate::copies::record(crate::copies::Site::MultipartImage, source.len());
        let pixels = if archive.layout.byte_order == Some(ByteOrder::Big)
            && let Some(word) = word_bytes(pixel_format)
        {
            let mut pixels = Vec::with_capacity(source.len());
            for sample in source.chunks_exact(word) {
                pixels.extend(sample.iter().rev().copied());
            }
            pixels
        } else {
            source.to_vec()
        };
        let mut layout = archive.layout.clone();
        layout.selected = part.index;
        layout.selection_required = false;
        layout.projected_format = Some(self);
        let mut selected = frame.clone();
        selected.jpeg = None;
        selected.width = width;
        selected.height = height;
        selected.pixel_format = pixel_format;
        selected.data = pixels.into();
        selected.multipart = Some(
            MultipartData {
                layout,
                payload: archive.payload.clone(),
            }
            .into(),
        );
        Ok(selected)
    }
}

/// An explicit ordinary metering source. The index is a table ordinal,
/// independent of source, purpose and component identities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartSelection {
    pub format: GvspPartFormat,
    #[serde(
        default,
        deserialize_with = "crate::config_number::optional_unsigned",
        skip_serializing_if = "Option::is_none"
    )]
    pub index: Option<u32>,
}

impl PartSelection {
    pub(crate) fn validate(self) -> Result<()> {
        GvspPartFormat::validate_index(self.index, Some(self.format))?;
        ensure!(
            self.format.ordinary(),
            Categorized::error(
                ErrorKind::Unsupported,
                "automatic image tuning requires an ordinary Mono8, RGB8 or YCbCr411_8 part"
            )
        );
        Ok(())
    }
}

/// References only: the source frame keeps ownership of the complete archive.
pub(crate) struct OrdinaryPart<'a> {
    data: &'a [u8],
    pub width: u32,
    pub height: u32,
    format: GvspPartFormat,
}

impl OrdinaryPart<'_> {
    pub(crate) fn at(&self, x: usize, y: usize) -> [u8; 3] {
        let width = self.width as usize;
        match self.format {
            GvspPartFormat::Mono8 => [self.data[y * width + x]; 3],
            GvspPartFormat::Rgb8 => *self.data[3 * (y * width + x)..]
                .first_chunk::<3>()
                .expect("validated RGB pixel"),
            GvspPartFormat::Ycbcr4118 => {
                let stride = crate::frame::row_bytes(self.width, 0x020c_005a);
                let p = &self.data[y * stride + (x / 4) * 6..];
                let p = p.first_chunk::<6>().expect("validated YCbCr411 group");
                crate::frame::ycbcr(p[[0, 1, 3, 4][x & 3]], p[2], p[5])
            }
            _ => unreachable!("ordinary part was validated before borrowing"),
        }
    }
}

/// The ordinary intensity subset supported by the native multipart receiver.
pub(super) fn gvsp_intensity_shape(primary: &Part) -> Result<(u32, u32, u32)> {
    let width = u32::try_from(primary.width.context("missing GVSP image width")?)?;
    let maximum = primary.height.context("missing GVSP image height")?;
    let height = u32::try_from(primary.delivered_height.unwrap_or(maximum))?;
    let format = u32::try_from(primary.format.context("missing GVSP image format")?)?;
    let info = primary.gvsp.context("missing GVSP part metadata")?;
    ensure!(
        primary.data_type == 1
            && primary.namespace.is_none()
            && info.zone_directions & 0x8000_0000 == 0
            && matches!(
                format,
                crate::types::MONO8 | crate::types::RGB8 | 0x020c_005a
            )
            && info.x_padding == Some(0)
            && info.y_padding == Some(0),
        Categorized::error(
            ErrorKind::Unsupported,
            "GVSP multipart intensity requires unpadded Mono8, RGB8 or YCbCr411_8"
        )
    );
    ensure!(
        width > 0
            && height > 0
            && usize::try_from(height)? <= maximum
            && crate::frame::frame_len(format, width, height) == Some(primary.length),
        "GVSP multipart intensity size disagrees with its geometry"
    );
    Ok((width, height, format))
}

pub(super) fn gvsp_delivered_shape(
    part: &Part,
    order: Option<ByteOrder>,
) -> Result<(u32, u32, u32)> {
    match GvspPartFormat::for_part(part) {
        Some(format) => format.shape(part, order),
        None if part.data_type == 1 && matches!(part.format, Some(0x0108_0001 | 0x020c_005a)) => {
            gvsp_intensity_shape(part)
        }
        None => Err(Categorized::error(
            ErrorKind::Unsupported,
            "GVSP delivered part requires a supported unpadded row format",
        )),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Layout {
    /// Omitted for existing `GenTL` part data type codes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_type_namespace: Option<PartTypeNamespace>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub byte_order: Option<ByteOrder>,
    /// The retained pixels are only a bounded internal representative.
    /// A job must project an exact part before interpreting or saving them.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub selection_required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub projected_format: Option<GvspPartFormat>,
    pub selected: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_buffer_bytes: Option<usize>,
    pub original_filled_bytes: usize,
    pub payload_bytes: usize,
    pub parts: Vec<Part>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gendc: Option<gendc::Container>,
}
impl Layout {
    pub(crate) fn validate_ranges(&self, bytes: usize) -> Result<()> {
        ensure!(
            (1..=MAX_PARTS).contains(&self.parts.len()),
            "multipart part count is outside 1..32"
        );
        ensure!(
            bytes > 0 && bytes <= MAX_BYTES && self.payload_bytes == bytes,
            "invalid multipart archive size"
        );
        ensure!(
            !self.selection_required || (self.gendc.is_none() && self.projected_format.is_none()),
            "deferred multipart selection contradicts a projected or GenDC layout"
        );
        if let Some(container) = &self.gendc {
            return container.validate_ranges(self, bytes);
        }
        let announced = match self.data_type_namespace {
            Some(PartTypeNamespace::GenDc) => anyhow::bail!("missing GenDC container metadata"),
            None => {
                let bytes = self
                    .original_buffer_bytes
                    .context("multipart announced buffer size is missing")?;
                ensure!(
                    (1..=MAX_BYTES).contains(&bytes),
                    "invalid multipart announced buffer size"
                );
                Some(bytes)
            }
            Some(PartTypeNamespace::Gvsp) => {
                ensure!(
                    self.original_buffer_bytes.is_none(),
                    "GVSP multipart has no announced buffer"
                );
                None
            }
        };
        ensure!(
            self.original_filled_bytes == bytes,
            "multipart archive does not account for every written byte"
        );
        let mut stored = 0usize;
        let mut wire = 0usize;
        let mut ranges = Vec::with_capacity(self.parts.len());
        for (index, part) in self.parts.iter().enumerate() {
            ensure!(
                usize::try_from(part.index)? == index && part.stored_offset == stored,
                "multipart part storage index is inconsistent"
            );
            ensure!(part.gendc.is_none(), "unexpected GenDC part metadata");
            if let Some(size) = announced {
                ensure!(part.gvsp.is_none(), "GenTL part has unexpected GVSP fields");
                let start = part
                    .original_offset
                    .context("multipart part source offset is missing")?;
                let end = start
                    .checked_add(part.length)
                    .context("multipart source range overflow")?;
                ensure!(end <= size, "multipart part exceeds announced buffer");
                if part.length > 0 {
                    ranges.push((start, end));
                }
            } else {
                let info = part.gvsp.context("missing GVSP part metadata")?;
                let declared = info.declared_length.unwrap_or(part.length);
                ensure!(
                    part.original_offset.is_none()
                        && info.address_offset == wire
                        && part.length <= declared,
                    "GVSP multipart part metadata is inconsistent"
                );
                wire = wire
                    .checked_add(declared)
                    .context("GVSP declared part extent overflow")?;
                ensure!(wire <= MAX_BYTES, "GVSP declared parts exceed 128 MiB");
                if info.declared_length.is_some() {
                    ensure!(
                        declared > part.length,
                        "GVSP shortened part must retain a larger declared length"
                    );
                    let (width, _, format) = gvsp_delivered_shape(part, self.byte_order)?;
                    let maximum =
                        u32::try_from(part.height.context("missing GVSP declared height")?)?;
                    ensure!(
                        crate::frame::frame_len(format, width, maximum) == Some(declared),
                        "GVSP declared part length disagrees with its maximum geometry"
                    );
                }
            }
            stored = stored
                .checked_add(part.length)
                .context("multipart archive size overflow")?;
            ensure!(stored <= bytes, "multipart part exceeds archive");
        }
        ranges.sort_unstable();
        ensure!(
            ranges
                .iter()
                .zip(ranges.iter().skip(1))
                .all(|(left, right)| left.1 <= right.0),
            "overlapping multipart parts are unsupported"
        );
        ensure!(
            stored == bytes,
            "multipart archive contains unexplained bytes"
        );
        Ok(())
    }

    pub(crate) fn validate(&self, bytes: usize) -> Result<()> {
        self.validate_ranges(bytes)?;
        let selected = self.selected_part()?;
        if let Some(format) = self.projected_format {
            ensure!(
                !self.selection_required && self.gendc.is_none() && format.matches(selected),
                "multipart projection metadata contradicts its selected descriptor"
            );
        }
        if let Some(container) = &self.gendc {
            container.image_shape(selected)?;
            return Ok(());
        }
        if self.data_type_namespace == Some(PartTypeNamespace::Gvsp)
            && let Some(format) = GvspPartFormat::for_part(selected)
        {
            format.shape(selected, self.byte_order)?;
            return Ok(());
        }
        ensure!(
            selected.image_type_supported() && selected.length > 0,
            Categorized::error(
                ErrorKind::Unsupported,
                "selected multipart component is not a supported image or scalar map"
            )
        );
        Ok(())
    }
    pub(crate) fn image_shape(&self, part: &Part) -> Result<(u32, u32, u32)> {
        self.gendc.as_ref().map_or_else(
            || part.image_shape(self.byte_order),
            |container| container.image_shape(part),
        )
    }

    pub(crate) fn selected_native_part_format(&self) -> Option<GvspPartFormat> {
        if self.data_type_namespace != Some(PartTypeNamespace::Gvsp) {
            return None;
        }
        GvspPartFormat::for_part(self.selected_part().ok()?).filter(|format| !format.ordinary())
    }

    pub(crate) const fn storage(&self) -> &'static str {
        if self.gendc.is_some() {
            "linear_gendc_container"
        } else {
            "concatenated_parts"
        }
    }

    pub(crate) fn selected_part(&self) -> Result<&Part> {
        self.parts
            .get(usize::try_from(self.selected)?)
            .context("multipart selected part is unavailable")
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct MultipartData {
    #[serde(flatten)]
    pub layout: Layout,
    #[serde(skip)]
    pub payload: FrameBuf,
}
impl MultipartData {
    pub(crate) fn retain(layout: Layout, raw: PoolVec) -> Result<(Self, PoolVec)> {
        layout.validate(raw.len())?;
        let selected = layout.selected_part()?;
        let end = selected
            .stored_offset
            .checked_add(selected.length)
            .context("multipart selected range overflow")?;
        let source = &raw[selected.stored_offset..end];
        let rows = layout
            .gendc
            .as_ref()
            .map(|container| container.image_rows(selected))
            .transpose()?;
        let pixel_bytes = rows.as_ref().map_or(source.len(), |rows| rows.pixel_bytes);
        let mut pixels = pool().take_stale(pixel_bytes);
        crate::copies::record(crate::copies::Site::MultipartImage, pixel_bytes);
        if let Some(rows) = rows.filter(|rows| rows.row_bytes != rows.stride) {
            for (output, input) in pixels
                .chunks_exact_mut(rows.row_bytes)
                .zip(source.chunks_exact(rows.stride))
            {
                output.copy_from_slice(&input[..rows.row_bytes]);
            }
        } else {
            pixels.copy_from_slice(&source[..pixel_bytes]);
        }
        Ok((
            Self {
                layout,
                payload: raw.freeze(),
            },
            pixels,
        ))
    }
    pub(crate) fn buffer_bytes(&self) -> usize {
        self.payload
            .allocated_bytes()
            .saturating_add(size_of::<Self>())
            .saturating_add(
                self.layout
                    .parts
                    .capacity()
                    .saturating_mul(size_of::<Part>()),
            )
            .saturating_add(self.layout.gendc.as_ref().map_or(0, |container| {
                container
                    .components
                    .capacity()
                    .saturating_mul(size_of::<gendc::Component>())
            }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn layout() -> Layout {
        serde_json::from_value(serde_json::json!({
            "selected":1,"byte_order":"big","original_buffer_bytes":64,
            "original_filled_bytes":4,"payload_bytes":4,
            "parts":[
                {"index":0,"original_offset":0,"stored_offset":0,"length":2,"data_type":0},
                {"index":1,"original_offset":32,"stored_offset":2,"length":2,"data_type":1}
            ]
        }))
        .unwrap()
    }
    #[test]
    fn shortened_gvsp_layout_distinguishes_declared_wire_ranges_from_compact_storage() {
        let value = serde_json::json!({
            "data_type_namespace":"gvsp", "selected":0, "byte_order":"little",
            "original_filled_bytes":24,"payload_bytes":24,
            "parts":[
                {"index":0,"stored_offset":0,"length":16,"data_type":5,"format":0x0110_00b8_u32,
                 "width":4,"height":5,"delivered_height":2,
                 "gvsp":{"address_offset":0,"declared_length":40,"zone_directions":0,"x_padding":0,"y_padding":0}},
                {"index":1,"stored_offset":16,"length":8,"data_type":1,"format":crate::types::MONO8,
                 "width":4,"height":5,"delivered_height":2,
                 "gvsp":{"address_offset":40,"declared_length":20,"zone_directions":0,"x_padding":0,"y_padding":0}}
            ]
        });
        let layout: Layout = serde_json::from_value(value.clone()).unwrap();
        layout.validate(24).unwrap();
        assert_eq!(
            gvsp_delivered_shape(&layout.parts[0], layout.byte_order).unwrap(),
            (4, 2, 0x0110_00b8)
        );
        assert_eq!(
            gvsp_delivered_shape(&layout.parts[1], layout.byte_order).unwrap(),
            (4, 2, crate::types::MONO8)
        );
        for mode in 0..8 {
            let mut bad = value.clone();
            match mode {
                0 => bad["parts"][1]["gvsp"]["address_offset"] = serde_json::json!(16),
                1 => {
                    bad["parts"][0]["gvsp"]
                        .as_object_mut()
                        .unwrap()
                        .remove("declared_length");
                }
                2 => bad["parts"][0]["gvsp"]["declared_length"] = serde_json::json!(16),
                3 => bad["parts"][0]["gvsp"]["declared_length"] = serde_json::json!(MAX_BYTES + 1),
                4 => bad["parts"][0]["height"] = serde_json::json!(6),
                5 => bad["parts"][0]["delivered_height"] = serde_json::json!(3),
                6 => bad["parts"][0]["data_type"] = serde_json::json!(0x8001),
                7 => bad["parts"][0]["gvsp"]["x_padding"] = serde_json::json!(1),
                _ => unreachable!(),
            }
            serde_json::from_value::<Layout>(bad)
                .unwrap()
                .validate(24)
                .unwrap_err();
        }
    }

    #[test]
    fn scalar_map_shape_requires_the_matching_pfnc32_part_type() {
        let mut part = layout().parts[1];
        part.namespace = Some(4);
        part.width = Some(1);
        part.height = Some(1);
        part.x_padding_pixels = Some(0);
        for (data_type, format) in [(5, 0x0110_00b8_u32), (9, 0x0110_00c7)] {
            part.data_type = data_type;
            part.format = Some(u64::from(format));
            assert_eq!(
                part.image_shape(Some(ByteOrder::Big)).unwrap(),
                (1, 1, format)
            );
            part.image_shape(None).unwrap_err();
            part.namespace = Some(3);
            part.image_shape(Some(ByteOrder::Little)).unwrap_err();
            part.namespace = Some(4);
            part.data_type = if data_type == 5 { 9 } else { 5 };
            part.image_shape(Some(ByteOrder::Little)).unwrap_err();
        }
    }

    #[test]
    fn interleaved_cloud_requires_three_binary32_words_per_pixel() {
        let mut part = layout().parts[1];
        part.data_type = 5;
        part.namespace = Some(4);
        part.format = Some(u64::from(crate::frame::COORD3D_ABC32F));
        part.length = 24;
        part.width = Some(2);
        part.height = Some(1);
        part.x_padding_pixels = Some(0);
        assert_eq!(
            part.image_shape(Some(ByteOrder::Big)).unwrap(),
            (2, 1, crate::frame::COORD3D_ABC32F)
        );
        part.image_shape(None).unwrap_err();
        part.data_type = 1;
        part.image_shape(Some(ByteOrder::Little)).unwrap_err();
        part.data_type = 5;
        part.length = 23;
        part.image_shape(Some(ByteOrder::Little)).unwrap_err();
        part.length = 24;
        part.format = Some(0x0260_00c1);
        part.image_shape(Some(ByteOrder::Little)).unwrap_err();
    }

    #[test]
    fn interleaved_abc16_requires_three_unsigned_words_and_standard_namespace() {
        let mut part = layout().parts[1];
        part.data_type = 5;
        part.namespace = Some(4);
        part.format = Some(u64::from(crate::frame::COORD3D_ABC16));
        part.width = Some(2);
        part.height = Some(1);
        part.x_padding_pixels = Some(0);
        part.length = 12;
        assert_eq!(
            part.image_shape(Some(ByteOrder::Big)).unwrap(),
            (2, 1, crate::frame::COORD3D_ABC16)
        );
        part.image_shape(None).unwrap_err();
        part.namespace = Some(1);
        part.image_shape(Some(ByteOrder::Little)).unwrap_err();
        part.namespace = Some(4);
        part.data_type = 1;
        part.image_shape(Some(ByteOrder::Little)).unwrap_err();
        part.data_type = 5;
        part.length = 11;
        part.image_shape(Some(ByteOrder::Little)).unwrap_err();
        part.length = 12;
        part.format = Some(0x0230_00ba);
        part.image_shape(Some(ByteOrder::Little)).unwrap_err();
    }

    #[test]
    fn interleaved_ac16_requires_3d_type_and_two_16_bit_words_per_point() {
        let mut part = layout().parts[1];
        part.data_type = 5;
        part.namespace = Some(4);
        part.format = Some(0x0220_00bb);
        part.width = Some(2);
        part.height = Some(1);
        part.x_padding_pixels = Some(0);
        part.length = 8;
        assert_eq!(
            part.image_shape(Some(ByteOrder::Big)).unwrap(),
            (2, 1, crate::frame::COORD3D_AC16)
        );
        part.image_shape(None).unwrap_err();
        part.data_type = 1;
        part.image_shape(Some(ByteOrder::Little)).unwrap_err();
        part.data_type = 5;
        part.length = 7;
        part.image_shape(Some(ByteOrder::Little)).unwrap_err();
    }

    #[test]
    fn retained_allocation_capacity_is_charged_beside_the_selected_pixels() {
        let mut payload = Vec::with_capacity(4096);
        payload.extend_from_slice(&[0xa1, 0xa2, 3, 4]);
        let payload: FrameBuf = payload.into();
        let allocation = payload.allocated_bytes();
        assert!(allocation >= 4096);
        let mut frame = crate::types::Frame {
            data: vec![3, 4].into(),
            ..crate::types::Frame::default()
        };
        let selected = frame.buffer_bytes();
        frame.multipart = Some(std::sync::Arc::new(MultipartData {
            layout: layout(),
            payload,
        }));
        assert!(frame.buffer_bytes() >= selected + allocation + size_of::<MultipartData>());
    }

    #[test]
    fn range_archive_retains_every_component_without_reading_allocation_gaps() {
        let mut raw = pool().take_stale(4);
        raw.copy_from_slice(&[0xa1, 0xa2, 3, 4]);
        let pointer = raw.as_ptr();
        let (parts, mut pixels) = MultipartData::retain(layout(), raw).unwrap();
        assert_eq!(parts.payload.as_ptr(), pointer);
        assert_eq!(parts.payload.as_ref(), [0xa1, 0xa2, 3, 4]);
        assert_eq!(&*pixels, [3, 4]);
        pixels[0] = 9;
        assert_eq!(parts.payload.as_ref(), [0xa1, 0xa2, 3, 4]);
        assert_eq!(parts.layout.parts[1].original_offset, Some(32));
        assert_eq!(parts.layout.parts[1].stored_offset, 2);
        assert!(
            parts.buffer_bytes() >= parts.payload.allocated_bytes() + size_of::<MultipartData>()
        );
    }
    #[test]
    fn malformed_archive_indexes_and_unaccounted_bytes_fail_before_selection_copy() {
        for mode in 0..7 {
            let mut layout = layout();
            match mode {
                0 => layout.parts[1].stored_offset = 1,
                1 => layout.parts[1].original_offset = Some(1),
                2 => layout.parts[1].original_offset = Some(usize::MAX),
                3 => layout.original_filled_bytes = 5,
                4 => layout.parts[1].length = 3,
                5 => layout.selected = 2,
                6 => layout.parts[1].index = 0,
                _ => unreachable!(),
            }
            layout.validate(4).unwrap_err();
        }
        let mut layout = layout();
        layout.selected = 0;
        let error = layout.validate(4).unwrap_err();
        assert_eq!(crate::types::classify(&error).kind, ErrorKind::Unsupported);
    }

    #[test]
    fn generic_gentl_bicolor_parts_reject_odd10p_before_byte_extent_checks() {
        for (code, width, length) in [
            (0x0214_00a8_u32, 4, 20),
            (0x0214_00aa, 4, 20),
            (0x0218_00ac, 3, 18),
            (0x0218_00ae, 3, 18),
        ] {
            let mut part: Part = serde_json::from_value(serde_json::json!({
                "index":0,"stored_offset":0,"length":length,"data_type":1,
                "format":code,"namespace":4,"width":width,"height":2,"x_padding_pixels":0
            }))
            .unwrap();
            assert_eq!(part.image_shape(None).unwrap(), (width, 2, code));
            if width == 4 {
                for (width, height, length) in [(3, 2, 16), (1, 1, 3)] {
                    part.width = Some(width);
                    part.height = Some(height);
                    part.length = length;
                    let error = part.image_shape(Some(ByteOrder::Little)).unwrap_err();
                    assert_eq!(crate::types::classify(&error).kind, ErrorKind::Unsupported);
                    assert!(error.to_string().contains("even source width"));
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "multipart_job_tests.rs"]
pub(crate) mod job_selection_tests;
