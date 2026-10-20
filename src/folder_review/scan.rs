//! Find a folder's media and decode the frames a review compares. Stills are
//! one frame each; videos are sampled every `interval_ms`. The folder is only
//! ever read.
use crate::{media::ReplayDecoder, transport::simulator::replay, types::Frame};
use anyhow::{Context, Result, bail, ensure};
use std::{
    fs,
    path::{Component, Path, PathBuf},
    time::{Duration, UNIX_EPOCH},
};

/// Deeper folders are almost always a link loop or a mistake.
const MAX_DEPTH: usize = 32;
/// More media than this is a library, not one review; review a subfolder.
pub(crate) const MAX_FILES: usize = 20_000;
/// Directory entries looked at before giving up, media or not.
const MAX_ENTRIES: usize = 1_000_000;
#[cfg(feature = "hubs")]
const MAX_STILL_BYTES: u64 = 256 * 1024 * 1024;
/// The box FFmpeg scales frames into, keeping their aspect ratio. Video
/// samples and the frame shown for one use the same box.
pub(crate) const VIDEO_BOX: (u32, u32) = (960, 540);
const STILL_BOX: (u32, u32) = (1920, 1080);
const FRAME_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Image,
    Video,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Video => "video",
        }
    }
    pub fn parse(name: &str) -> Result<Self> {
        match name {
            "image" => Ok(Self::Image),
            "video" => Ok(Self::Video),
            other => bail!("unsupported folder review file kind {other}"),
        }
    }
}

/// A media file found in the folder, with what identifies its content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Found {
    /// Relative to the folder, with `/` separators.
    pub path: String,
    pub kind: Kind,
    pub bytes: u64,
    pub modified_ns: u64,
}

#[derive(Debug, Default)]
pub(crate) struct Walk {
    pub files: Vec<Found>,
    /// Media whose names are not UTF-8, which a session cannot record.
    pub skipped: u64,
}

/// Every supported regular file below `folder`, sorted by path. Symbolic
/// links and hidden entries are skipped, never followed.
pub(crate) fn walk(folder: &Path) -> Result<Walk> {
    let mut walk = Walk::default();
    let mut entries = 0usize;
    let mut stack = vec![(folder.to_path_buf(), 0usize)];
    while let Some((directory, depth)) = stack.pop() {
        let listing = fs::read_dir(&directory)
            .with_context(|| format!("cannot list {}", directory.display()))?;
        for entry in listing {
            let entry = entry.with_context(|| format!("cannot list {}", directory.display()))?;
            entries += 1;
            ensure!(
                entries <= MAX_ENTRIES,
                "folder review stops after {MAX_ENTRIES} folder entries; review a subfolder"
            );
            if entry.file_name().as_encoded_bytes().starts_with(b".") {
                continue;
            }
            let path = entry.path();
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                if depth < MAX_DEPTH {
                    stack.push((path, depth + 1));
                }
                continue;
            }
            if !file_type.is_file() || !replay::supported(&path) {
                continue;
            }
            let Some(relative) = relative(folder, &path) else {
                walk.skipped += 1;
                continue;
            };
            let metadata = entry.metadata()?;
            walk.files.push(Found {
                path: relative,
                kind: if replay::is_image(&path) {
                    Kind::Image
                } else {
                    Kind::Video
                },
                bytes: metadata.len(),
                modified_ns: modified_ns(&metadata),
            });
            ensure!(
                walk.files.len() <= MAX_FILES,
                "folder holds more than {MAX_FILES} images and videos; review a subfolder"
            );
        }
    }
    walk.files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(walk)
}

fn relative(folder: &Path, path: &Path) -> Option<String> {
    let parts: Vec<&str> = path
        .strip_prefix(folder)
        .ok()?
        .components()
        .map(|part| match part {
            Component::Normal(name) => name.to_str(),
            _ => None,
        })
        .collect::<Option<_>>()?;
    Some(parts.join("/"))
}

pub(crate) fn modified_ns(metadata: &fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |time| time.as_nanos().min(u128::from(u64::MAX)) as u64)
}

/// A recorded relative path inside `folder`. A modified session cannot
/// point outside it, and the file must still be a regular one.
pub(crate) fn source(folder: &Path, relative: &str) -> Result<PathBuf> {
    let path = Path::new(relative);
    ensure!(
        !relative.is_empty()
            && path
                .components()
                .all(|part| matches!(part, Component::Normal(_))),
        "invalid file path in folder review session"
    );
    Ok(folder.join(path))
}

/// The file as recorded: still there, still a regular file, same size and
/// modification time. `Err` says why not.
pub(crate) fn unchanged(path: &Path, bytes: u64, modified: u64) -> Result<(), &'static str> {
    match fs::symlink_metadata(path) {
        Err(_) => Err("is missing"),
        Ok(metadata) if !metadata.file_type().is_file() => Err("is no longer a regular file"),
        Ok(metadata) if metadata.len() != bytes || modified_ns(&metadata) != modified => {
            Err("changed since it was scanned")
        }
        Ok(_) => Ok(()),
    }
}

/// Decode a still image: JPEG and PNG in process when this build has the
/// decoders, anything else through FFmpeg.
pub(crate) fn still(path: &Path) -> Result<Frame> {
    #[cfg(feature = "hubs")]
    {
        let extension = path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if matches!(extension.as_str(), "jpg" | "jpeg" | "png") {
            use std::io::Read;
            let mut bytes = Vec::new();
            fs::File::open(path)
                .with_context(|| format!("cannot open {}", path.display()))?
                .take(MAX_STILL_BYTES + 1)
                .read_to_end(&mut bytes)?;
            ensure!(
                bytes.len() as u64 <= MAX_STILL_BYTES,
                "image is larger than 256 MiB"
            );
            return crate::hubs::image::decode(&bytes).map_err(|reason| anyhow::anyhow!(reason));
        }
    }
    let mut decoder =
        ReplayDecoder::open(path, STILL_BOX.0, STILL_BOX.1, 1.0, true, FRAME_TIMEOUT)?;
    decoder
        .next(FRAME_TIMEOUT)?
        .context("the image holds no frame")
}

/// Decode a video's samples, one every `interval_ms`, calling `each` with the
/// offset and frame until it returns `false`, the video ends or `limit`
/// samples are taken. Each sample is a seek to its offset, exactly as
/// `video_frame` shows it, so what is ranked is what is reviewed.
pub(crate) fn video(
    path: &Path,
    interval_ms: u64,
    limit: usize,
    mut each: impl FnMut(u64, Frame) -> Result<bool>,
) -> Result<()> {
    for index in 0..limit as u64 {
        let offset = index * interval_ms;
        let mut decoder =
            ReplayDecoder::frame_at(path, VIDEO_BOX.0, VIDEO_BOX.1, offset, FRAME_TIMEOUT)?;
        let Some(frame) = decoder.next(FRAME_TIMEOUT)? else {
            ensure!(index > 0, "the video holds no frame");
            break;
        };
        if !each(offset, frame)? {
            break;
        }
    }
    Ok(())
}

/// The video frame sampled at `offset_ms`.
pub(crate) fn video_frame(path: &Path, offset_ms: u64) -> Result<Frame> {
    let mut decoder =
        ReplayDecoder::frame_at(path, VIDEO_BOX.0, VIDEO_BOX.1, offset_ms, FRAME_TIMEOUT)?;
    decoder
        .next(FRAME_TIMEOUT)?
        .context("the video has no frame at that time")
}
