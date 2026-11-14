//! Opaque `GenICam` chunk capture. Chunk IDs are defined by the camera's XML;
//! retaining bytes and offsets lets a consumer interpret them without guessing
//! vendor meanings or treating measurement bytes as image padding.
//!
//! `GenApi` 2.1.1 section 2.8.16 defines chunk lengths excluding their trailers:
//! <https://www.emva.org/wp-content/uploads/GenICam_Standard_v2_1_1.pdf>
//! The ID/length trailer fields use the transport's byte order.
use crate::framebuf::{FrameBuf, PoolVec, pool};
use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};

/// A bounded metadata document even if a corrupt stream contains tiny chunks.
const MAX_CHUNKS: usize = 4096;
const MAX_PAYLOAD: usize = 128 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ByteOrder {
    Big,
    Little,
}
impl ByteOrder {
    const fn u32(self, bytes: [u8; 4]) -> u32 {
        match self {
            Self::Big => u32::from_be_bytes(bytes),
            Self::Little => u32::from_le_bytes(bytes),
        }
    }
}

/// One chunk's data in the original payload, excluding any transport footer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chunk {
    pub id: u64,
    pub offset: usize,
    pub length: usize,
}

/// The original image's layout, before row compaction and sample byte swaps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageLayout {
    /// Byte offset of the original image in the complete retained payload.
    #[serde(default)]
    pub offset: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pfnc_code: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pixel_encoding: Option<crate::types::PixelEncoding>,
    pub width: u32,
    pub height: u32,
    pub row_bytes: usize,
    pub row_stride: usize,
    pub padding_y: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub byte_order: Option<ByteOrder>,
    pub continuous_packing: bool,
}

/// Complete camera payload and the information needed to recover its chunks.
/// `Frame.data` remains the normalized pixels used by image consumers. This
/// separate buffer preserves the image, transport padding and every chunk
/// footer exactly as received, even when pixel normalization changes the image.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkData {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout_id: Option<u64>,
    /// Footer byte order, absent when a producer supplies decoded descriptors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub byte_order: Option<ByteOrder>,
    pub image_bytes: usize,
    /// Footer-chain start, absent for a producer-provided descriptor index.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunk_offset: Option<usize>,
    pub payload_bytes: usize,
    pub entries: Vec<Chunk>,
    /// Opaque capture remains available when the producer cannot index chunks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub descriptor_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_layout: Option<ImageLayout>,
    /// XML-interpreted per-frame values and explicit unavailable diagnostics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub values: Option<serde_json::Value>,
    #[serde(skip)]
    pub payload: FrameBuf,
}
impl ChunkData {
    /// Resident bytes retained by a queued frame, including interpretation
    /// strings and descriptor storage rather than just the camera payload.
    pub(crate) fn buffer_bytes(&self) -> usize {
        self.payload
            .allocated_bytes()
            .saturating_add(size_of::<Self>())
            .saturating_add(self.entries.capacity().saturating_mul(size_of::<Chunk>()))
            .saturating_add(self.values.as_ref().map_or(0, value_heap_bytes))
            .saturating_add(self.descriptor_error.as_ref().map_or(0, String::capacity))
    }

    /// Validate the footer chain in `chunk_offset..payload.len()`. USB3 Vision
    /// may include the image's footer immediately after its pixels; accepting
    /// that footer does not require knowing the camera-specific image `ChunkID`.
    pub(crate) fn parse(
        payload: &[u8],
        image_bytes: usize,
        chunk_offset: usize,
        byte_order: ByteOrder,
        layout_id: u32,
    ) -> Result<Self> {
        ensure!(
            payload.len() <= MAX_PAYLOAD,
            "GenICam chunk payload exceeds 128 MiB limit"
        );
        ensure!(
            image_bytes <= chunk_offset && chunk_offset <= payload.len(),
            "chunk payload does not contain the complete image"
        );
        let mut end = payload.len();
        let mut entries = Vec::new();
        while end > chunk_offset {
            ensure!(entries.len() < MAX_CHUNKS, "too many GenICam chunks");
            let footer = end
                .checked_sub(8)
                .context("truncated GenICam chunk footer")?;
            ensure!(footer >= chunk_offset, "truncated GenICam chunk footer");
            let id = byte_order.u32(payload[footer..footer + 4].try_into()?);
            let length = usize::try_from(byte_order.u32(payload[footer + 4..end].try_into()?))
                .context("GenICam chunk exceeds host address size")?;
            let offset = footer
                .checked_sub(length)
                .context("GenICam chunk length exceeds payload")?;
            // In image-extended-chunk mode the image can itself be the first
            // chunk. Its footer is still retained and described in the index.
            let image_footer = footer == chunk_offset && offset == 0 && length == image_bytes;
            ensure!(
                offset >= chunk_offset || image_footer,
                "GenICam chunk overlaps image data"
            );
            entries.push(Chunk {
                id: u64::from(id),
                offset,
                length,
            });
            end = if image_footer { chunk_offset } else { offset };
        }
        entries.reverse();
        Ok(Self {
            layout_id: Some(u64::from(layout_id)),
            byte_order: Some(byte_order),
            image_bytes,
            chunk_offset: Some(chunk_offset),
            payload_bytes: payload.len(),
            entries,
            descriptor_error: None,
            image_layout: None,
            values: None,
            payload: FrameBuf::default(),
        })
    }

    /// A producer-supplied index uses offsets from the complete buffer, not
    /// a transport footer chain. Descriptors may overlap or repeat an ID;
    /// their exact representation is retained for XML interpretation.
    #[cfg(any(test, feature = "gentl"))]
    pub(crate) fn indexed(
        payload: &[u8],
        image_bytes: usize,
        image_layout: ImageLayout,
        layout_id: Option<u64>,
        entries: Vec<Chunk>,
    ) -> Result<Self> {
        ensure!(
            payload.len() <= MAX_PAYLOAD,
            "GenICam chunk payload exceeds 128 MiB limit"
        );
        ensure!(entries.len() <= MAX_CHUNKS, "too many GenICam chunks");
        ensure!(
            image_bytes > 0
                && image_layout
                    .offset
                    .checked_add(image_bytes)
                    .is_some_and(|end| end <= payload.len()),
            "indexed payload does not contain the complete image"
        );
        for entry in &entries {
            ensure!(
                entry
                    .offset
                    .checked_add(entry.length)
                    .is_some_and(|end| end <= payload.len()),
                "chunk descriptor exceeds retained payload"
            );
        }
        Ok(Self {
            layout_id: layout_id.filter(|id| *id != 0),
            byte_order: None,
            image_bytes,
            chunk_offset: None,
            payload_bytes: payload.len(),
            entries,
            descriptor_error: None,
            image_layout: Some(image_layout),
            values: None,
            payload: FrameBuf::default(),
        })
    }

    /// Retain the receive buffer unchanged and give image normalization a
    /// buffer of its own. The extra copy is confined to chunk-enabled frames.
    pub(crate) fn retain(&mut self, mut payload: PoolVec) -> PoolVec {
        payload.truncate(self.payload_bytes);
        let image = self.copy_image(&payload);
        self.payload = payload.freeze();
        image
    }

    #[cfg(feature = "usb")]
    pub(crate) fn retain_shared(&mut self, payload: FrameBuf) -> FrameBuf {
        let image = self.copy_image(&payload).freeze();
        self.payload = payload;
        image
    }

    fn copy_image(&self, payload: &[u8]) -> PoolVec {
        crate::copies::record(crate::copies::Site::ChunkImage, self.image_bytes);
        let mut image = pool().take_stale(self.image_bytes);
        let offset = self.image_layout.as_ref().map_or(0, |layout| layout.offset);
        image.copy_from_slice(&payload[offset..offset + self.image_bytes]);
        image
    }
}

fn value_heap_bytes(value: &serde_json::Value) -> usize {
    use serde_json::Value;
    match value {
        Value::String(text) => text.capacity(),
        // Numeric chunk values are bounded GenApi integers/floats. Include
        // token storage retained by arbitrary_precision numbers as well.
        Value::Number(number) => number.as_str().len().saturating_mul(2).saturating_add(64),
        Value::Array(values) => values.iter().fold(
            values.capacity().saturating_mul(size_of::<Value>()),
            |bytes, value| bytes.saturating_add(value_heap_bytes(value)),
        ),
        Value::Object(values) => {
            // A small B-tree node has unused key slots; a conservative bound
            // includes those slots and node pointers, including sparse maps.
            let slots = values.len().saturating_add(1).saturating_mul(3).max(11);
            let storage =
                slots.saturating_mul(size_of::<(String, Value)>() + 3 * size_of::<usize>());
            values.iter().fold(storage, |bytes, (name, value)| {
                bytes
                    .saturating_add(name.capacity())
                    .saturating_add(value_heap_bytes(value))
            })
        }
        Value::Null | Value::Bool(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(bytes: &mut Vec<u8>, data: &[u8], id: u32, order: ByteOrder) {
        bytes.extend_from_slice(data);
        for value in [id, u32::try_from(data.len()).unwrap()] {
            bytes.extend_from_slice(&match order {
                ByteOrder::Big => value.to_be_bytes(),
                ByteOrder::Little => value.to_le_bytes(),
            });
        }
    }

    #[test]
    fn short_chunk_payload_counts_reserved_backing_but_keeps_logical_wire_sizes() {
        let mut wire = vec![1, 2];
        chunk(&mut wire, &[3, 4], 19, ByteOrder::Little);
        let mut parsed = ChunkData::parse(&wire, 2, 2, ByteOrder::Little, 42).unwrap();
        let pool = crate::framebuf::FramePool::new(|| None);
        let mut received = pool.take_empty(1 << 20);
        received.extend_from_slice(&wire);
        let allocated = received.capacity();
        let address = received.as_ptr();
        let image = parsed.retain(received);
        assert_eq!(&*image, &[1, 2]);
        assert_eq!(parsed.payload.as_ref(), wire);
        assert_eq!(parsed.payload.as_ptr(), address);
        assert_eq!(parsed.payload.len(), wire.len());
        assert_eq!(parsed.payload_bytes, wire.len());
        assert_eq!(parsed.image_bytes, 2);
        assert_eq!(parsed.payload.allocated_bytes(), allocated);
        assert_eq!(
            parsed.buffer_bytes(),
            allocated + size_of::<ChunkData>() + parsed.entries.capacity() * size_of::<Chunk>()
        );
        assert_eq!(
            serde_json::to_value(&parsed).unwrap()["payload_bytes"],
            wire.len()
        );
    }

    #[test]
    fn unknown_ids_empty_chunks_and_image_footer_are_preserved_in_order() {
        for order in [ByteOrder::Big, ByteOrder::Little] {
            let mut payload = vec![1, 2, 3, 4];
            chunk(&mut payload, &[], 0xffff_ff01, order);
            chunk(&mut payload, &[9, 8, 7], 12, order);
            let mut parsed = ChunkData::parse(&payload, 4, 4, order, 67).unwrap();
            assert_eq!(
                parsed.entries,
                vec![
                    Chunk {
                        id: 0xffff_ff01,
                        offset: 4,
                        length: 0
                    },
                    Chunk {
                        id: 12,
                        offset: 12,
                        length: 3
                    },
                ]
            );
            let mut received = pool().take_stale(payload.len() + 32);
            received.fill(0xa5);
            received[..payload.len()].copy_from_slice(&payload);
            let image = parsed.retain(received);
            assert_eq!(&*image, &[1, 2, 3, 4]);
            assert_eq!(&*parsed.payload, &payload);
            assert_eq!(
                serde_json::to_value(&parsed).unwrap()["payload_bytes"],
                payload.len()
            );
            assert!(
                serde_json::to_value(&parsed)
                    .unwrap()
                    .get("payload")
                    .is_none()
            );

            let mut image_chunk = Vec::new();
            chunk(&mut image_chunk, &[1, 2, 3, 4], 1, order);
            chunk(&mut image_chunk, &[2], 2, order);
            let parsed = ChunkData::parse(&image_chunk, 4, 4, order, 0).unwrap();
            assert_eq!(
                parsed.entries[0],
                Chunk {
                    id: 1,
                    offset: 0,
                    length: 4
                }
            );
        }
    }

    fn indexed_layout(offset: usize) -> ImageLayout {
        ImageLayout {
            pixel_encoding: None,
            offset,
            pfnc_code: Some(crate::types::MONO8),
            width: 3,
            height: 1,
            row_bytes: 3,
            row_stride: 3,
            padding_y: 0,
            byte_order: None,
            continuous_packing: false,
        }
    }

    #[test]
    fn indexed_chunks_keep_wide_ids_offsets_and_overlapping_descriptors() {
        let bytes = [0xaa, 0xbb, 1, 2, 3, 0xcc, 0xdd];
        let entries = vec![
            Chunk {
                id: u64::MAX,
                offset: 5,
                length: 2,
            },
            Chunk {
                id: u64::MAX,
                offset: 0,
                length: 4,
            },
        ];
        let mut chunks = ChunkData::indexed(
            &bytes,
            3,
            indexed_layout(2),
            Some(u64::MAX),
            entries.clone(),
        )
        .unwrap();
        let mut receive = pool().take_stale(bytes.len());
        receive.copy_from_slice(&bytes);
        assert_eq!(&*chunks.retain(receive), &[1, 2, 3]);
        assert_eq!(chunks.payload.as_ref(), bytes);
        assert_eq!(chunks.entries, entries);
        let json = serde_json::to_value(&chunks).unwrap();
        assert_eq!(json["layout_id"].as_u64(), Some(u64::MAX));
        assert_eq!(json["entries"][0]["id"].as_u64(), Some(u64::MAX));
        assert_eq!(json["image_layout"]["offset"], 2);
        assert!(json.get("byte_order").is_none());
        assert!(json.get("chunk_offset").is_none());
        assert!(json["image_layout"].get("byte_order").is_none());
        assert!(json.get("payload").is_none());
    }

    #[test]
    fn indexed_chunks_reject_overflowing_spans_and_indexes() {
        let bytes = [1, 2, 3];
        for entry in [
            Chunk {
                id: 1,
                offset: usize::MAX,
                length: 2,
            },
            Chunk {
                id: 1,
                offset: 2,
                length: 2,
            },
        ] {
            ChunkData::indexed(&bytes, 3, indexed_layout(0), None, vec![entry]).unwrap_err();
        }
        ChunkData::indexed(&bytes, 3, indexed_layout(usize::MAX), None, Vec::new()).unwrap_err();
        ChunkData::indexed(&bytes, 0, indexed_layout(0), None, Vec::new()).unwrap_err();
        ChunkData::indexed(
            &bytes,
            3,
            indexed_layout(0),
            None,
            vec![
                Chunk {
                    id: 1,
                    offset: 0,
                    length: 0
                };
                MAX_CHUNKS + 1
            ],
        )
        .unwrap_err();
        let chunks = ChunkData::indexed(&bytes, 3, indexed_layout(0), Some(0), Vec::new()).unwrap();
        assert_eq!(chunks.layout_id, None);
    }

    #[test]
    fn malformed_lengths_and_unbounded_indexes_are_rejected() {
        let mut payload = vec![0; 4];
        chunk(&mut payload, &[8], 1, ByteOrder::Little);
        for length in [2, u32::MAX] {
            let end = payload.len();
            payload[end - 4..].copy_from_slice(&length.to_le_bytes());
            ChunkData::parse(&payload, 4, 4, ByteOrder::Little, 0).unwrap_err();
        }
        for end in 5..12 {
            ChunkData::parse(&payload[..end], 4, 4, ByteOrder::Little, 0).unwrap_err();
        }
        let payload = vec![0; (MAX_CHUNKS + 1) * 8];
        ChunkData::parse(&payload, 0, 0, ByteOrder::Big, 0).unwrap_err();
        ChunkData::parse(&[0; 4], 5, 4, ByteOrder::Big, 0).unwrap_err();
    }
}
