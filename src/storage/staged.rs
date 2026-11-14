//! Bounded streaming artifacts, published only after a complete acquisition.
use super::{Cut, Folders, Recording, SyncMode, folder, partial, publish};
use crate::types::{Categorized, ErrorKind};
use anyhow::{Context as _, Result, ensure};
use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::Arc,
};

/// A same-volume hidden recording. Drop/abort cleans unpublished data; finish
/// never replaces a destination and rolls back a failed publication it owns.
pub(crate) struct StagedRecording {
    recording: Option<Recording>,
    temporary: PathBuf,
    destination: PathBuf,
    min_free: Option<u64>,
    owns_destination: bool,
}
impl StagedRecording {
    pub(crate) fn create(path: &Path, limit: Option<u64>, min_free: Option<u64>) -> Result<Self> {
        crate::naming::check_free(path)?;
        drop(folder(path, 0, SyncMode::File, min_free)?);
        let temporary = partial(path)?;
        let recording = Recording::create(&temporary, limit, min_free)?;
        Ok(Self {
            recording: Some(recording),
            temporary,
            destination: path.to_owned(),
            min_free,
            owns_destination: false,
        })
    }
    pub(crate) fn bytes_written(&self) -> u64 {
        self.recording.as_ref().map_or(0, Recording::bytes_written)
    }
    pub(crate) fn alarm(&self) -> Arc<super::Alarm> {
        self.recording
            .as_ref()
            .expect("staged recording is open")
            .alarm()
    }
    pub(crate) fn finish(mut self) -> Result<u64> {
        match self.publish() {
            Ok(bytes) => {
                self.owns_destination = false;
                Ok(bytes)
            }
            Err(error) => {
                let cleaned = self.cleanup();
                Err(match cleaned {
                    Ok(()) => error,
                    Err(cleanup) => {
                        error.context(format!("artifact cleanup also failed: {cleanup:#}"))
                    }
                })
            }
        }
    }
    fn publish(&mut self) -> Result<u64> {
        let mut recording = self.recording.take().context("artifact already finished")?;
        if let Some(error) = recording.alarm().error() {
            return Err(error);
        }
        ensure!(
            recording.cut().is_none(),
            Categorized::error(
                ErrorKind::Incomplete,
                match recording.cut() {
                    Some(Cut::MaxSize) => "staged artifact exceeded its size limit",
                    _ => "staged artifact has a failed write",
                }
            )
        );
        drop(folder(&self.destination, 0, SyncMode::File, self.min_free)?);
        crate::naming::check_free(&self.destination)?;
        let file = recording
            .file
            .take()
            .context("artifact file already closed")?;
        file.sync_all()
            .with_context(|| format!("cannot flush artifact {}", self.destination.display()))?;
        drop(file);
        let bytes = recording.bytes_written();
        if publish(&self.temporary, &self.destination)
            .map_err(|error| super::failed(&self.destination, error))?
        {
            self.owns_destination = true;
        } else {
            self.copy_publish(bytes)?;
        }
        self.remove_stage()?;
        Folders::of(&self.destination).sync()?;
        Ok(bytes)
    }
    /// The same create-new fallback as still storage, streaming through the
    /// free-space accounting on volumes without no-replace rename/hard links.
    fn copy_publish(&mut self, expected: u64) -> Result<()> {
        drop(folder(
            &self.destination,
            expected,
            SyncMode::File,
            self.min_free,
        )?);
        let mut output = Recording::create(&self.destination, None, self.min_free)?;
        self.owns_destination = true;
        let mut input = fs::File::open(&self.temporary)?;
        let copied = io::copy(&mut input, &mut output)?;
        if let Some(error) = output.alarm().error() {
            return Err(error);
        }
        ensure!(
            copied == expected,
            "staged artifact changed before publication"
        );
        output.finish()?;
        Ok(())
    }
    fn remove_stage(&self) -> Result<()> {
        match fs::remove_file(&self.temporary) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => {
                Err(error).with_context(|| {
                    format!("cannot remove staged artifact {}", self.temporary.display())
                })
            }
            _ => Ok(()),
        }
    }
    fn cleanup(&mut self) -> Result<()> {
        drop(self.recording.take());
        let stage = self.remove_stage();
        let destination = if self.owns_destination {
            match fs::remove_file(&self.destination) {
                Ok(()) => {
                    self.owns_destination = false;
                    Ok(())
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    self.owns_destination = false;
                    Ok(())
                }
                Err(error) => Err(error).with_context(|| {
                    format!(
                        "cannot remove failed artifact {}",
                        self.destination.display()
                    )
                }),
            }
        } else {
            Ok(())
        };
        stage.and(destination)
    }
    pub(crate) fn abort(mut self) -> Result<()> {
        self.cleanup()
    }
}
impl io::Write for StagedRecording {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        io::Write::write(
            self.recording
                .as_mut()
                .ok_or_else(|| io::Error::other("artifact already finished"))?,
            bytes,
        )
    }
    fn flush(&mut self) -> io::Result<()> {
        io::Write::flush(
            self.recording
                .as_mut()
                .ok_or_else(|| io::Error::other("artifact already finished"))?,
        )
    }
}
impl Drop for StagedRecording {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    #[test]
    fn complete_stream_is_hidden_until_published_and_durable() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("trace.bin");
        let mut stage = StagedRecording::create(&path, Some(128), Some(0)).unwrap();
        stage.write_all(b"header").unwrap();
        stage.write_all(b"data").unwrap();
        assert_eq!(stage.bytes_written(), 10);
        assert!(!path.exists());
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
        assert_eq!(stage.finish().unwrap(), 10);
        assert_eq!(fs::read(&path).unwrap(), b"headerdata");
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }
    #[test]
    fn aborted_and_dropped_stages_publish_nothing() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("trace.bin");
        let mut stage = StagedRecording::create(&path, None, Some(0)).unwrap();
        stage.write_all(b"incomplete").unwrap();
        stage.abort().unwrap();
        drop(StagedRecording::create(&path, None, Some(0)).unwrap());
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
    }
    #[test]
    fn publication_races_never_remove_someone_elses_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("trace.bin");
        let mut stage = StagedRecording::create(&path, None, Some(0)).unwrap();
        stage.write_all(b"ours").unwrap();
        fs::write(&path, b"theirs").unwrap();
        let error = stage.finish().unwrap_err();
        assert!(error.downcast_ref::<super::super::OutputExists>().is_some());
        assert_eq!(fs::read(&path).unwrap(), b"theirs");
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }
    #[test]
    fn a_cut_artifact_is_never_published() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("trace.bin");
        let mut stage = StagedRecording::create(&path, Some(2), Some(0)).unwrap();
        stage.write_all(b"long").unwrap_err();
        let error = stage.finish().unwrap_err();
        assert_eq!(crate::types::classify(&error).kind, ErrorKind::Incomplete);
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
    }
}
