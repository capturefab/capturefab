//! Companion artifacts for camera payloads that contain opaque `GenICam` chunks.
//! Roll back only files this attempt published when another artifact fails.
use crate::{storage, types::Frame};
use anyhow::{Context as _, Result, ensure};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

#[derive(Default)]
pub(super) struct Artifacts {
    paths: Vec<PathBuf>,
    reference: Option<Value>,
}
impl Artifacts {
    pub(super) fn save(
        path: &Path,
        frame: &Frame,
        sync: storage::SyncMode,
        min_free: Option<u64>,
        hash: bool,
    ) -> Result<Self> {
        let mut artifacts = Self::default();
        if let Some(chunks) = &frame.chunks {
            ensure!(
                chunks.payload.len() == chunks.payload_bytes,
                crate::types::Categorized::error(
                    crate::types::ErrorKind::Incomplete,
                    "camera chunk payload bytes are missing"
                )
            );
            let payload_path = crate::naming::chunk_payload(path);
            let metadata_path = crate::naming::sidecar(&payload_path);
            let stored = storage::save(&chunks.payload, &payload_path, sync, min_free, hash)
                .with_context(|| format!("save camera chunks {}", payload_path.display()))?;
            artifacts.paths.push(payload_path.clone());
            let metadata = crate::types::without_nulls(json!({
                "format_version": 1,
                "image_path": super::absolute_path(path),
                "frame_id": frame.id,
                "device_ts_ns": frame.device_ts_ns,
                "device_timestamp": frame.device_timestamp,
                "received_monotonic_ns": frame.received_monotonic_ns,
                "wall_ms": frame.wall_ms,
                "chunks": chunks.as_ref(),
                "payload_path": super::absolute_path(&payload_path),
                "sha256": stored.sha256,
            }));
            let metadata = serde_json::to_vec(&metadata)?;
            let _ = storage::save(&metadata, &metadata_path, sync, min_free, false)
                .with_context(|| format!("save chunk metadata {}", metadata_path.display()))?;
            artifacts.paths.push(metadata_path.clone());
            artifacts.reference = Some(crate::types::without_nulls(json!({
                "format_version": 1,
                "path": super::absolute_path(&payload_path),
                "metadata_path": super::absolute_path(&metadata_path),
                "bytes": stored.bytes,
                "sha256": stored.sha256,
                "layout_id": chunks.layout_id,
                "count": chunks.entries.len(),
                "descriptor_error": chunks.descriptor_error,
            })));
        }
        Ok(artifacts)
    }

    pub(super) const fn reference(&self) -> Option<&Value> {
        self.reference.as_ref()
    }

    pub(super) fn commit(mut self) -> Option<Value> {
        self.paths.clear();
        self.reference.take()
    }
}
impl Drop for Artifacts {
    fn drop(&mut self) {
        for path in &self.paths {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::chunks::{ByteOrder, Chunk, ChunkData, ImageLayout};

    fn frame() -> Frame {
        Frame {
            pixel_encoding: None,
            id: 7,
            width: 2,
            height: 1,
            pixel_format: crate::types::MONO8,
            data: vec![3, 4].into(),
            chunks: Some(
                ChunkData {
                    layout_id: Some(42),
                    byte_order: Some(ByteOrder::Big),
                    image_bytes: 2,
                    chunk_offset: Some(2),
                    payload_bytes: 12,
                    descriptor_error: None,
                    entries: vec![Chunk {
                        id: 19,
                        offset: 2,
                        length: 2,
                    }],
                    payload: vec![3, 4, 0x12, 0x34, 0, 0, 0, 19, 0, 0, 0, 2].into(),
                    values: None,
                    image_layout: Some(ImageLayout {
                        pixel_encoding: None,
                        offset: 0,
                        pfnc_code: Some(crate::types::MONO8),
                        width: 2,
                        height: 1,
                        row_bytes: 2,
                        row_stride: 2,
                        padding_y: 0,
                        byte_order: Some(ByteOrder::Big),
                        continuous_packing: false,
                    }),
                }
                .into(),
            ),
            ..Frame::default()
        }
    }

    #[test]
    fn chunk_artifacts_preserve_wire_bytes_and_reconstructible_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image.png");
        let frame = frame();
        let saved = Artifacts::save(&path, &frame, storage::SyncMode::Off, Some(0), true).unwrap();
        let reference = saved.commit().unwrap();
        let payload = Path::new(reference["path"].as_str().unwrap());
        assert_eq!(
            std::fs::read(payload).unwrap(),
            frame.chunks.as_ref().unwrap().payload.as_ref()
        );
        let metadata: Value = serde_json::from_slice(
            &std::fs::read(reference["metadata_path"].as_str().unwrap()).unwrap(),
        )
        .unwrap();
        assert_eq!(
            metadata["chunks"]["entries"][0],
            json!({"id":19,"offset":2,"length":2})
        );
        assert_eq!(metadata["chunks"]["image_layout"]["row_stride"], 2);
        assert_eq!(metadata["sha256"], reference["sha256"]);
        assert!(metadata["chunks"].get("payload").is_none());
    }

    #[test]
    fn failed_group_removes_only_owned_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image.raw");
        let payload = crate::naming::chunk_payload(&path);
        let metadata = crate::naming::sidecar(&payload);
        std::fs::write(&metadata, b"existing").unwrap();
        assert!(Artifacts::save(&path, &frame(), storage::SyncMode::Off, Some(0), false).is_err());
        assert!(!payload.exists());
        assert_eq!(std::fs::read(&metadata).unwrap(), b"existing");
        std::fs::remove_file(metadata).unwrap();
        drop(Artifacts::save(&path, &frame(), storage::SyncMode::Off, Some(0), false).unwrap());
        assert!(!payload.exists());
    }
}
