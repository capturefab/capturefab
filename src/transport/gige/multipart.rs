//! Checked placement for extended-ID GVSP multipart blocks.
use super::{Data, GvspPacket, be16, be32, be64, ticks_ns};
use crate::{
    copies::{self, Site},
    framebuf::{PoolVec, pool},
    transport::{
        chunks::ByteOrder,
        multipart::{
            GvspPart, GvspPartFormat, Layout, MAX_PARTS, MultipartData, Part, PartTypeNamespace,
            gvsp_delivered_shape, gvsp_intensity_shape, word_bytes,
        },
    },
    types::{Categorized, DeviceTimestamp, ErrorKind, Frame},
};
use anyhow::{Context as _, Result, ensure};
use std::collections::BTreeMap;

pub(super) const PAYLOAD_TYPE: u16 = 0x000a;
pub(super) const CONTENT: u8 = 7;
const LEADER_BYTES: usize = 12;
const LEADER_PART_BYTES: usize = 48;
// Metadata datagrams may be larger than the negotiated data-packet size.
#[cfg(any(test, target_os = "linux"))]
pub(super) const MAX_METADATA_DATAGRAM: usize = 20 + LEADER_BYTES + MAX_PARTS * LEADER_PART_BYTES;
const TRAILER_BYTES: usize = 4;
const TRAILER_PART_BYTES: usize = 16;
pub(super) const FRAGMENT_HEADER: usize = 8;
const MAX_EARLY_BYTES: usize = 64 * 1024;
const MAX_EARLY_PACKETS: usize = 128;

pub(super) const fn is_metadata(packet: GvspPacket<'_>) -> bool {
    matches!(packet.kind, 1 | 2)
        && packet.data.len() >= 4
        && be16(packet.data.bytes(), 2) & 0x3fff == PAYLOAD_TYPE
}

fn unsupported(message: &str) -> anyhow::Error {
    Categorized::error(ErrorKind::Unsupported, message)
}

/// The native 48-bit size and block address fields are big endian.
const fn be48(data: &[u8], offset: usize) -> u64 {
    u64::from_be_bytes([
        0,
        0,
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
        data[offset + 4],
        data[offset + 5],
    ])
}

#[derive(PartialEq, Eq)]
struct Leader {
    timestamp: u64,
    layout: Layout,
}

impl Leader {
    fn parse(
        packet: GvspPacket<'_>,
        limit: usize,
        order: Option<ByteOrder>,
        projection: bool,
    ) -> Result<Self> {
        let data = packet.data.bytes();
        let count = usize::from(packet.part_count);
        ensure!(
            (1..=MAX_PARTS).contains(&count)
                && data.len() == LEADER_BYTES + count * LEADER_PART_BYTES,
            "GVSP multipart leader part count does not match its bounded table"
        );
        ensure!(
            be16(data, 2) == PAYLOAD_TYPE,
            unsupported("GVSP multipart with chunks or a device-specific payload is unsupported")
        );
        let mut stored = 0usize;
        let mut parts = Vec::with_capacity(count);
        for (index, record) in data[LEADER_BYTES..]
            .as_chunks::<LEADER_PART_BYTES>()
            .0
            .iter()
            .enumerate()
        {
            let part = parse_part(record, u32::try_from(index)?, stored, limit)?;
            stored += part.length;
            parts.push(part);
        }
        // A custom pixel code carries no standard image interpretation. The
        // documented stereo error map is retained as opaque bytes.
        let mut images = parts.iter().filter(|part| {
            part.data_type == 1
                && part.length > 0
                && part.format.is_some_and(|format| {
                    format & 0x8000_0000 == 0
                        // Mono16 can carry intensity, range or confidence.
                        && !matches!(format, 0x0108_00b1 | 0x0108_00c6 | 0x0110_0007)
                        && part.measurement_format().is_none()
                })
        });
        let primary = images.next();
        let ambiguous = images.next().is_some();
        ensure!(
            !ambiguous || projection,
            unsupported(
                "GVSP multipart requires exactly one ordinary intensity image; configure a single Intensity component"
            )
        );
        let selected = if ambiguous {
            representative(&parts, order)?.0
        } else if let Some(primary) = primary {
            gvsp_intensity_shape(primary)?;
            primary.index
        } else {
            representative(&parts, order)?.0
        };
        let layout = Layout {
            selection_required: ambiguous || primary.is_none(),
            projected_format: None,
            data_type_namespace: Some(PartTypeNamespace::Gvsp),
            byte_order: order,
            selected,
            original_buffer_bytes: None,
            original_filled_bytes: stored,
            payload_bytes: stored,
            parts,
            gendc: None,
        };
        // A selected part's delivered height is confirmed
        // by its trailer. The leader proves the declared geometry and ranges.
        layout.validate_ranges(stored)?;
        Ok(Self {
            timestamp: be64(data, 4),
            layout,
        })
    }
}

fn representative(parts: &[Part], order: Option<ByteOrder>) -> Result<(u32, (u32, u32, u32))> {
    let mut first_error = None;
    for part in parts {
        let Some(format) = GvspPartFormat::for_part(part) else {
            continue;
        };
        match format.declared_shape(part, order) {
            Ok(shape) => return Ok((part.index, shape)),
            Err(error) if first_error.is_none() => first_error = Some(error),
            Err(_) => {}
        }
    }
    Err(first_error.unwrap_or_else(|| {
        unsupported("GVSP multipart needs a supported intensity or selectable part")
    }))
}

fn parse_part(
    record: &[u8; LEADER_PART_BYTES],
    index: u32,
    stored: usize,
    limit: usize,
) -> Result<Part> {
    let data_type = usize::from(be16(record, 0));
    ensure!(
        data_type != 10,
        unsupported("GVSP multipart chunk parts are unsupported")
    );
    ensure!(
        record[15] & 0x1f == 0 && be32(record, 16) & 0x8000_0000 == 0,
        unsupported("GVSP multipart requires a single top-down zone per part")
    );
    let length =
        usize::try_from(be48(record, 2)).context("GVSP part length exceeds host address size")?;
    let end = stored
        .checked_add(length)
        .context("GVSP multipart size overflow")?;
    ensure!(
        end <= limit,
        "GVSP multipart exceeds configured payload size"
    );
    let image = (1..=9).contains(&data_type);
    Ok(Part {
        index,
        original_offset: None,
        stored_offset: stored,
        length,
        data_type,
        format: Some(u64::from(be32(record, 8))),
        namespace: None,
        width: image.then(|| be32(record, 24) as usize),
        height: image.then(|| be32(record, 28) as usize),
        delivered_height: None,
        x_offset: image.then(|| be32(record, 32) as usize),
        y_offset: image.then(|| be32(record, 36) as usize),
        x_padding_pixels: None,
        source_id: Some(u64::from(record[14])),
        region_id: Some(u64::from(be16(record, 22))),
        purpose_id: Some(u64::from(be16(record, 20))),
        gvsp: Some(GvspPart {
            address_offset: stored,
            declared_length: None,
            zone_directions: be32(record, 16),
            x_padding: image.then(|| be16(record, 40)),
            y_padding: image.then(|| be16(record, 42)),
            type_specific_leader: (!image)
                .then(|| std::array::from_fn(|word| be32(record, 24 + word * 4))),
            type_specific_trailer: None,
        }),
        gendc: None,
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct TrailerPart {
    data_type: usize,
    length: u64,
    words: [u32; 2],
}
#[derive(PartialEq, Eq)]
struct Trailer {
    id: u32,
    parts: Vec<TrailerPart>,
}
impl Trailer {
    fn parse(id: u32, data: &[u8]) -> Result<Self> {
        let table = data
            .get(TRAILER_BYTES..)
            .context("truncated GVSP multipart trailer")?;
        let count = table.len() / TRAILER_PART_BYTES;
        ensure!(
            (1..=MAX_PARTS).contains(&count) && table.len().is_multiple_of(TRAILER_PART_BYTES),
            "GVSP multipart trailer has an invalid part table"
        );
        ensure!(
            be16(data, 2) == PAYLOAD_TYPE,
            unsupported("GVSP multipart with chunks or a device-specific payload is unsupported")
        );
        let parts = table
            .as_chunks::<TRAILER_PART_BYTES>()
            .0
            .iter()
            .map(|record| TrailerPart {
                data_type: usize::from(be16(record, 0)),
                length: be48(record, 2),
                words: [be32(record, 8), be32(record, 12)],
            })
            .collect();
        Ok(Self { id, parts })
    }

    fn check(&self, layout: &Layout) -> Result<()> {
        ensure!(
            self.parts.len() == layout.parts.len(),
            "GVSP multipart trailer part count disagrees with leader"
        );
        for (tail, head) in self.parts.iter().zip(&layout.parts) {
            delivered_part(head, tail, layout.byte_order)?;
        }
        Ok(())
    }

    fn check_received(&self, layout: &Layout, spans: &BTreeMap<usize, (usize, u32)>) -> Result<()> {
        for (&start, &(end, _)) in spans {
            ensure!(
                layout.parts.iter().zip(&self.parts).any(|(part, tail)| {
                    let base = part.gvsp.map(|info| info.address_offset);
                    base.is_some_and(|base| {
                        start >= base
                            && u64::try_from(end - base).is_ok_and(|length| length <= tail.length)
                    })
                }),
                unsupported(
                    "GVSP multipart tail/padding bytes outside a delivered prefix are unsupported"
                )
            );
        }
        Ok(())
    }
}

fn delivered_part(head: &Part, tail: &TrailerPart, order: Option<ByteOrder>) -> Result<Part> {
    ensure!(
        tail.data_type == head.data_type
            && tail.length <= u64::try_from(head.length)?
            && head
                .height
                .is_none_or(|height| usize::try_from(tail.words[0])
                    .is_ok_and(|delivered| delivered <= height)),
        "GVSP multipart trailer disagrees with its leader"
    );
    let mut actual = *head;
    actual.length = usize::try_from(tail.length)?;
    let info = actual.gvsp.as_mut().context("missing GVSP part metadata")?;
    if let Some(height) = head.height {
        actual.delivered_height = Some(usize::try_from(tail.words[0])?);
        if actual.length != head.length || actual.delivered_height != Some(height) {
            info.declared_length = (actual.length < head.length).then_some(head.length);
            // Only known unpadded row layouts can prove a shortened prefix.
            gvsp_delivered_shape(&actual, order)?;
        }
    } else {
        ensure!(
            actual.length == head.length,
            unsupported("GVSP opaque parts must keep their declared length")
        );
        info.type_specific_trailer = Some(tail.words);
    }
    Ok(actual)
}

struct EarlyPacket {
    part: usize,
    offset: usize,
    data: Vec<u8>,
}

#[derive(Default)]
pub(super) struct MultipartFrame {
    leader: Option<Leader>,
    trailer: Option<Trailer>,
    archive: Option<PoolVec>,
    prepared: Option<Frame>,
    early: BTreeMap<u32, EarlyPacket>,
    staged_bytes: usize,
    /// Disjoint archive ranges keyed by byte start, with wire packet ID.
    spans: BTreeMap<usize, (usize, u32)>,
    covered: usize,
    pub(super) highest: u32,
}

impl MultipartFrame {
    pub(super) const fn has_leader(&self) -> bool {
        self.leader.is_some()
    }

    pub(super) const fn has_trailer(&self) -> bool {
        self.trailer.is_some()
    }

    pub(super) fn coverage_complete(&self) -> bool {
        let Some(leader) = &self.leader else {
            return false;
        };
        self.trailer
            .as_ref()
            .map_or(self.covered == leader.layout.payload_bytes, |trailer| {
                trailer.parts.iter().try_fold(0usize, |size, part| {
                    size.checked_add(usize::try_from(part.length).ok()?)
                }) == Some(self.covered)
            })
    }

    pub(super) const fn is_prepared(&self) -> bool {
        self.prepared.is_some()
    }

    pub(super) fn receive_leader(
        &mut self,
        packet: GvspPacket<'_>,
        limit: usize,
        order: Option<ByteOrder>,
        projection: bool,
    ) -> Result<()> {
        ensure!(
            packet.id == 0,
            "GVSP multipart leader packet ID must be zero"
        );
        let leader = Leader::parse(packet, limit, order, projection)?;
        if let Some(previous) = &self.leader {
            ensure!(
                previous == &leader,
                "conflicting duplicate GVSP multipart leader"
            );
            return Ok(());
        }
        if let Some(trailer) = &self.trailer {
            trailer.check(&leader.layout)?;
        }
        self.archive = Some(pool().take_stale(leader.layout.payload_bytes));
        self.leader = Some(leader);
        for (id, early) in std::mem::take(&mut self.early) {
            self.place(id, early.part, early.offset, &early.data, false)?;
        }
        self.staged_bytes = 0;
        Ok(())
    }

    pub(super) fn receive_trailer(&mut self, id: u32, data: &[u8]) -> Result<()> {
        ensure!(
            id >= 2 && id > self.highest,
            "GVSP multipart trailer ID precedes payload"
        );
        let trailer = Trailer::parse(id, data)?;
        if let Some(previous) = &self.trailer {
            ensure!(
                previous == &trailer,
                "conflicting duplicate GVSP multipart trailer"
            );
        } else {
            if let Some(leader) = &self.leader {
                trailer.check(&leader.layout)?;
                trailer.check_received(&leader.layout, &self.spans)?;
            }
            self.trailer = Some(trailer);
        }
        Ok(())
    }

    /// Bytes newly accepted, which may be staged until the leader arrives.
    pub(super) fn receive_fragment(
        &mut self,
        packet: GvspPacket<'_>,
        max_data: usize,
        limit: usize,
        duplicate: bool,
    ) -> Result<usize> {
        let Data::Bytes(bytes, _) = packet.data else {
            anyhow::bail!("GVSP multipart fragment landed in a predicted image slot");
        };
        let (header, data) = bytes
            .split_first_chunk::<FRAGMENT_HEADER>()
            .context("invalid GVSP multipart fragment size or ID")?;
        ensure!(
            packet.id > 0 && !data.is_empty() && data.len() <= max_data,
            "invalid GVSP multipart fragment size or ID"
        );
        let part = usize::from(header[0]);
        ensure!(part < MAX_PARTS, "GVSP multipart part ID exceeds bounds");
        ensure!(
            header[1] & 0x3e == 0,
            unsupported("GVSP multipart requires a single zone per part")
        );
        let offset = usize::try_from(be48(header, 2))
            .context("GVSP part offset exceeds host address size")?;
        ensure!(
            offset
                .checked_add(data.len())
                .is_some_and(|end| end <= limit),
            "GVSP multipart fragment exceeds configured payload bounds"
        );
        if self.leader.is_some() {
            self.place(packet.id, part, offset, data, duplicate)?;
        } else if duplicate {
            ensure!(
                self.early.get(&packet.id).is_some_and(|early| {
                    (early.part, early.offset, early.data.as_slice()) == (part, offset, data)
                }),
                "conflicting duplicate GVSP multipart fragment"
            );
        } else {
            if self.early.len() == MAX_EARLY_PACKETS
                || self.staged_bytes + data.len() > limit.min(MAX_EARLY_BYTES)
            {
                // No received bit is set. A later trailer can request this
                // packet with the ordinary missing-ID/resend policy.
                return Ok(0);
            }
            copies::record(Site::GvspMultipart, data.len());
            self.staged_bytes += data.len();
            self.early.insert(
                packet.id,
                EarlyPacket {
                    part,
                    offset,
                    data: data.to_vec(),
                },
            );
        }
        if duplicate {
            return Ok(0);
        }
        self.highest = self.highest.max(packet.id);
        Ok(data.len())
    }

    fn place(
        &mut self,
        id: u32,
        part: usize,
        offset: usize,
        bytes: &[u8],
        duplicate: bool,
    ) -> Result<()> {
        let leader = self
            .leader
            .as_ref()
            .context("GVSP multipart leader is missing")?;
        let part_index = part;
        let part = leader
            .layout
            .parts
            .get(part_index)
            .context("GVSP multipart fragment names an unknown part")?;
        // Aravis places this field relative to the start of the entire
        // payload block. A part ID restricts the range; it is not a base.
        let start = offset;
        let end = start
            .checked_add(bytes.len())
            .context("GVSP fragment end overflow")?;
        ensure!(
            start >= part.stored_offset && end <= part.stored_offset + part.length,
            "GVSP multipart fragment crosses its declared block address range"
        );
        if let Some(trailer) = &self.trailer {
            let length = usize::try_from(
                trailer
                    .parts
                    .get(part_index)
                    .context("missing GVSP trailer part")?
                    .length,
            )?;
            ensure!(
                end <= part.stored_offset + length,
                unsupported(
                    "GVSP multipart tail/padding bytes outside a delivered prefix are unsupported"
                )
            );
        }
        let archive = self
            .archive
            .as_mut()
            .context("GVSP multipart archive is missing")?;
        if duplicate {
            ensure!(
                self.spans.get(&start) == Some(&(end, id)) && archive[start..end] == *bytes,
                "conflicting duplicate GVSP multipart fragment"
            );
        } else {
            ensure!(
                self.spans
                    .range(..=start)
                    .next_back()
                    .is_none_or(|(_, (previous, _))| *previous <= start)
                    && self
                        .spans
                        .range(start..)
                        .next()
                        .is_none_or(|(next, _)| *next >= end),
                "overlapping GVSP multipart fragments"
            );
            copies::record(Site::GvspMultipart, bytes.len());
            archive[start..end].copy_from_slice(bytes);
            self.spans.insert(start, (end, id));
            self.covered += bytes.len();
        }
        Ok(())
    }

    /// Validate and materialize at the fallible receive boundary, before delivery.
    pub(super) fn prepare(&mut self, block: u64, frequency: u64) -> Result<()> {
        if self.prepared.is_some() {
            return Ok(());
        }
        let leader = self
            .leader
            .as_ref()
            .context("GVSP multipart leader is missing")?;
        let trailer = self
            .trailer
            .as_ref()
            .context("GVSP multipart trailer is missing")?;
        trailer.check(&leader.layout)?;
        trailer.check_received(&leader.layout, &self.spans)?;
        let mut layout = leader.layout.clone();
        let mut stored = 0usize;
        for ((part, head), tail) in layout
            .parts
            .iter_mut()
            .zip(&leader.layout.parts)
            .zip(&trailer.parts)
        {
            *part = delivered_part(head, tail, layout.byte_order)?;
            let start = part
                .gvsp
                .context("missing GVSP part metadata")?
                .address_offset;
            let end = start
                .checked_add(part.length)
                .context("GVSP delivered prefix overflow")?;
            let mut next = start;
            for (&first, &(last, _)) in self.spans.range(start..end) {
                ensure!(
                    first == next && last <= end,
                    "GVSP multipart contains a byte coverage hole"
                );
                next = last;
            }
            ensure!(next == end, "GVSP multipart contains a byte coverage hole");
            part.stored_offset = stored;
            stored = stored
                .checked_add(part.length)
                .context("GVSP retained prefix size overflow")?;
        }
        layout.payload_bytes = stored;
        layout.original_filled_bytes = stored;
        layout.validate(stored)?;
        let (width, height, pixel_format) =
            gvsp_delivered_shape(layout.selected_part()?, layout.byte_order)?;
        let timestamp = leader.timestamp;
        let mut raw = self
            .archive
            .take()
            .context("GVSP multipart archive is missing")?;
        for part in &layout.parts {
            let source = part
                .gvsp
                .context("missing GVSP part metadata")?
                .address_offset;
            if source != part.stored_offset && part.length > 0 {
                copies::record(Site::GvspMultipart, part.length);
                raw.copy_within(source..source + part.length, part.stored_offset);
            }
        }
        // Preserve the maximum backing allocation for queue budget charging.
        // Only received prefixes remain in the logical image/archive span.
        raw.truncate(stored);
        let (multipart, mut pixels) = MultipartData::retain(layout, raw)?;
        if multipart.layout.byte_order == Some(ByteOrder::Big)
            && let Some(word) = word_bytes(pixel_format)
        {
            pixels.chunks_exact_mut(word).for_each(<[u8]>::reverse);
        }
        self.prepared = Some(Frame {
            pixel_encoding: None,
            id: block,
            width,
            height,
            pixel_format,
            device_timestamp: Some(DeviceTimestamp {
                ticks: timestamp,
                frequency_hz: (frequency != 0).then_some(frequency),
            }),
            device_ts_ns: (frequency != 0).then(|| ticks_ns(timestamp, frequency)),
            data: pixels.freeze(),
            multipart: Some(multipart.into()),
            ..Frame::default()
        });
        Ok(())
    }

    pub(super) fn finish(self) -> Frame {
        self.prepared
            .expect("delivered multipart was prepared at receive boundary")
    }
}
