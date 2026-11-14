//! Configured context is an opaque byte file, never proof of identity or calibration.
use crate::types::Usage;
use anyhow::{Context as _, Result, ensure};
use std::{fs::File, io::Read as _, path::Path};

const MAX_CONTEXT: usize = 1024 * 1024;
#[derive(Debug)]
pub(super) struct Context {
    pub raw: Vec<u8>,
    pub supplied: bool,
}
impl Context {
    pub(super) fn read(path: Option<&Path>) -> Result<Self> {
        let Some(path) = path else {
            return Ok(Self {
                raw: Vec::new(),
                supplied: false,
            });
        };
        let mut raw = Vec::new();
        File::open(path)
            .context("open RoboSense configured context")?
            .take(u64::try_from(MAX_CONTEXT + 1)?)
            .read_to_end(&mut raw)?;
        ensure!(
            raw.len() <= MAX_CONTEXT,
            Usage("RoboSense configured context exceeds 1048576 bytes".into())
        );
        Ok(Self {
            raw,
            supplied: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn configured_context_keeps_csv_json_binary_and_empty_bytes_without_interpretation() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("calibration");
        for raw in [
            b"Laser id,Elevation,Azimuth\n1,15.0,0.0\n".to_vec(),
            b"{\"x\":1,\"x\":2}".to_vec(),
            vec![0, 128, 255],
            Vec::new(),
        ] {
            std::fs::write(&path, &raw).unwrap();
            let context = Context::read(Some(&path)).unwrap();
            assert_eq!(context.raw, raw);
            assert!(context.supplied);
        }
        let absent = Context::read(None).unwrap();
        assert!(!absent.supplied);
        assert_eq!(absent.raw, [] as [u8; 0]);
    }
    #[test]
    fn oversized_configured_context_stops_before_archive_creation() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("calibration");
        std::fs::write(&path, vec![0; MAX_CONTEXT + 1]).unwrap();
        let error = Context::read(Some(&path)).unwrap_err();
        assert_eq!(
            crate::types::classify(&error).kind,
            crate::types::ErrorKind::Usage
        );
    }
}
