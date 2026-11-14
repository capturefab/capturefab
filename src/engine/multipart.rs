//! Local still companions for complete retained multipart component archives.
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
        camera_id: &str,
        sync: storage::SyncMode,
        min_free: Option<u64>,
        hash: bool,
    ) -> Result<Self> {
        crate::frame::validate_encoding(frame)?;
        let mut saved = Self::default();
        if let Some(parts) = &frame.multipart {
            parts.layout.validate(parts.payload.len())?;
            let payload = crate::naming::multipart_payload(path, parts.layout.gendc.is_some());
            let metadata_path = crate::naming::sidecar(&payload);
            let stored = storage::save(&parts.payload, &payload, sync, min_free, hash)
                .with_context(|| format!("save multipart archive {}", payload.display()))?;
            saved.paths.push(payload.clone());
            let mut metadata = crate::types::without_nulls(json!({
                "format_version":1, "storage":parts.layout.storage(),
                "image_path":super::absolute_path(path),
                "payload_path":super::absolute_path(&payload),
                "frame_id":frame.id, "device_ts_ns":frame.device_ts_ns,
                "device_timestamp":frame.device_timestamp,
                "received_monotonic_ns":frame.received_monotonic_ns, "wall_ms":frame.wall_ms,
                "multipart":parts.as_ref(), "sha256":stored.sha256,
            }));
            if let Ok(selector) = crate::transport::gentl_selector::Selector::parse(camera_id) {
                let (choices, truncated) =
                    selector.image_selectors(&parts.layout, |part, format| {
                        crate::frame::is_measurement_format(format)
                            || (part.index != parts.layout.selected && part.has_gendc_padding())
                    });
                if !choices.is_empty() {
                    metadata["image_selectors"] = json!(choices);
                }
                if truncated > 0 {
                    metadata["image_selectors_truncated"] = json!(truncated);
                }
            }
            let bytes = serde_json::to_vec(&metadata)?;
            ensure!(
                bytes.len() <= 1024 * 1024,
                "multipart artifact metadata exceeds1 MiB"
            );
            let _ = storage::save(&bytes, &metadata_path, sync, min_free, false)
                .with_context(|| format!("save multipart metadata {}", metadata_path.display()))?;
            saved.paths.push(metadata_path.clone());
            saved.reference = Some(crate::types::without_nulls(json!({
                "format_version":1, "storage":parts.layout.storage(),
                "path":super::absolute_path(&payload),
                "metadata_path":super::absolute_path(&metadata_path),
                "bytes":stored.bytes, "sha256":stored.sha256,
                "selected":parts.layout.selected, "count":parts.layout.parts.len(),
                "part_format":parts.layout.projected_format,
            })));
        }
        Ok(saved)
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
    fn frame() -> Frame {
        let layout = serde_json::from_value(serde_json::json!({
            "selected":0,"byte_order":"little","original_buffer_bytes":8,
            "original_filled_bytes":2,"payload_bytes":2,
            "parts":[{"index":0,"original_offset":4,"stored_offset":0,"length":2,"data_type":1}]
        }))
        .unwrap();
        Frame {
            pixel_encoding: None,
            id: 9,
            width: 2,
            height: 1,
            pixel_format: crate::types::MONO8,
            data: vec![3, 4].into(),
            multipart: Some(std::sync::Arc::new(
                crate::transport::multipart::MultipartData {
                    layout,
                    payload: vec![3, 4].into(),
                },
            )),
            ..Frame::default()
        }
    }
    #[test]
    fn late_raw_sidecar_collision_removes_owned_image_and_component_artifacts() {
        let directory = tempfile::tempdir().unwrap();
        let image = directory.path().join("late.raw");
        let sidecar = crate::naming::sidecar(&image);
        let frame = frame();
        let artifacts =
            Artifacts::save(&image, &frame, "", storage::SyncMode::Off, Some(0), false).unwrap();
        std::fs::write(&image, &frame.data).unwrap();
        // A competing writer claims the raw sidecar after the companion precheck.
        std::fs::write(&sidecar, b"raced sidecar").unwrap();
        let error = super::super::save_sidecar(
            &image,
            &frame,
            0,
            &super::super::Acquisition::default(),
            storage::SyncMode::Off,
            Some(0),
            super::super::ArtifactReferences {
                chunks: None,
                multipart: artifacts.reference(),
            },
        )
        .unwrap_err();
        assert!(error.downcast_ref::<storage::OutputExists>().is_some());
        drop(artifacts);
        assert!(!image.exists());
        let payload = crate::naming::multipart_payload(&image, false);
        assert!(!payload.exists() && !crate::naming::sidecar(&payload).exists());
        assert_eq!(std::fs::read(sidecar).unwrap(), b"raced sidecar");
    }

    #[test]
    fn local_components_keep_reconstructible_references_and_roll_back_owned_artifacts() {
        let directory = tempfile::tempdir().unwrap();
        let image = directory.path().join("image.raw");
        let payload = crate::naming::multipart_payload(&image, false);
        let metadata = crate::naming::sidecar(&payload);
        std::fs::write(&metadata, b"existing").unwrap();
        let Err(error) =
            Artifacts::save(&image, &frame(), "", storage::SyncMode::Off, Some(0), false)
        else {
            panic!("companion collision must fail");
        };
        assert!(error.downcast_ref::<storage::OutputExists>().is_some());
        assert!(!payload.exists());
        assert_eq!(std::fs::read(&metadata).unwrap(), b"existing");
        std::fs::remove_file(&metadata).unwrap();
        drop(
            Artifacts::save(&image, &frame(), "", storage::SyncMode::Off, Some(0), false).unwrap(),
        );
        assert!(!payload.exists() && !metadata.exists());
        let reference =
            Artifacts::save(&image, &frame(), "", storage::SyncMode::Off, Some(0), true)
                .unwrap()
                .commit()
                .unwrap();
        assert_eq!(std::fs::read(&payload).unwrap(), [3, 4]);
        let meta: Value = serde_json::from_slice(&std::fs::read(&metadata).unwrap()).unwrap();
        assert_eq!(meta["storage"], "concatenated_parts");
        assert_eq!(meta["multipart"]["parts"][0]["original_offset"], 4);
        assert_eq!(meta["multipart"]["parts"][0]["stored_offset"], 0);
        assert_eq!(meta["multipart"]["byte_order"], "little");
        assert_eq!(meta["sha256"], reference["sha256"]);
        assert!(meta["multipart"].get("payload").is_none());
    }
}
