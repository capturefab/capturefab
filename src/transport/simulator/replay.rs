//! Local test-data playback for a virtual camera. Directory membership is a
//! snapshot of regular files directly inside the selected directory.
use crate::{
    media::ReplayDecoder,
    types::{Backend, Frame, RGB8, RegisterIo},
};
use anyhow::{Context, Result, bail, ensure};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

const XML: &str = r#"<?xml version="1.0"?>
<RegisterDescription ModelName="Replay camera" VendorName="Capturefab" StandardNameSpace="None">
 <Category Name="Root"><pFeature>DeviceControl</pFeature><pFeature>ImageFormatControl</pFeature><pFeature>AcquisitionControl</pFeature></Category>
 <Category Name="DeviceControl"><pFeature>DeviceScanType</pFeature></Category>
 <Enumeration Name="DeviceScanType"><Value>0</Value><ImposedAccessMode>RO</ImposedAccessMode><EnumEntry Name="Areascan"><Value>0</Value></EnumEntry></Enumeration>
 <Category Name="ImageFormatControl"><pFeature>Width</pFeature><pFeature>Height</pFeature><pFeature>PixelFormat</pFeature><pFeature>PayloadSize</pFeature></Category>
 <Integer Name="Width"><Description>Output width; test data is scaled to fit with black borders. Stop acquisition before changing.</Description><pValue>WidthReg</pValue><Min>64</Min><Max>1920</Max><Inc>1</Inc></Integer>
 <IntReg Name="WidthReg"><Address>0x100</Address><Length>4</Length><AccessMode>RW</AccessMode><Endianess>LittleEndian</Endianess><Sign>Unsigned</Sign></IntReg>
 <Integer Name="Height"><Description>Output height. Stop acquisition before changing.</Description><pValue>HeightReg</pValue><Min>64</Min><Max>1080</Max><Inc>1</Inc></Integer>
 <IntReg Name="HeightReg"><Address>0x104</Address><Length>4</Length><AccessMode>RW</AccessMode><Endianess>LittleEndian</Endianess><Sign>Unsigned</Sign></IntReg>
 <Enumeration Name="PixelFormat"><Value>0x02180014</Value><ImposedAccessMode>RO</ImposedAccessMode><EnumEntry Name="RGB8"><Value>0x02180014</Value></EnumEntry></Enumeration>
 <Integer Name="PayloadSize"><pValue>PayloadReg</pValue></Integer>
 <IntReg Name="PayloadReg"><Address>0x128</Address><Length>4</Length><AccessMode>RO</AccessMode><Endianess>LittleEndian</Endianess><Sign>Unsigned</Sign></IntReg>
 <Category Name="AcquisitionControl"><pFeature>AcquisitionFrameRate</pFeature><pFeature>AcquisitionStart</pFeature><pFeature>AcquisitionStop</pFeature></Category>
 <Float Name="AcquisitionFrameRate"><Description>Playback frames per second. Videos are resampled; each still filename supplies one frame. Stop acquisition before changing.</Description><pValue>RateReg</pValue><Min>1</Min><Max>120</Max><Unit>Hz</Unit></Float>
 <FloatReg Name="RateReg"><Address>0x120</Address><Length>8</Length><AccessMode>RW</AccessMode><Endianess>LittleEndian</Endianess></FloatReg>
 <Command Name="AcquisitionStart"><pValue>AcqReg</pValue><CommandValue>1</CommandValue></Command>
 <Command Name="AcquisitionStop"><pValue>AcqReg</pValue><CommandValue>0</CommandValue></Command>
 <IntReg Name="AcqReg"><Address>0x130</Address><Length>4</Length><AccessMode>WO</AccessMode><Endianess>LittleEndian</Endianess><Sign>Unsigned</Sign></IntReg>
</RegisterDescription>"#;

fn extension(path: &Path) -> String {
    path.extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}
fn is_image(path: &Path) -> bool {
    matches!(
        extension(path).as_str(),
        "png" | "jpg" | "jpeg" | "bmp" | "tif" | "tiff" | "webp" | "ppm" | "pgm" | "pbm" | "pnm"
    )
}
fn supported(path: &Path) -> bool {
    is_image(path)
        || matches!(
            extension(path).as_str(),
            "mp4"
                | "m4v"
                | "mov"
                | "mkv"
                | "avi"
                | "webm"
                | "mpg"
                | "mpeg"
                | "ts"
                | "mts"
                | "m2ts"
                | "vob"
                | "flv"
                | "wmv"
                | "ogv"
        )
}
fn regular_file(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("cannot inspect simulation media {}", path.display()))?;
    ensure!(
        metadata.file_type().is_file(),
        "simulation media must be a regular file, not a symlink: {}",
        path.display()
    );
    Ok(())
}

/// List supported direct children, sorted by filename. Subdirectories, symlinks,
/// and unrelated files are ignored; a symlink selected as the source is rejected.
pub fn source_files(source: &Path) -> Result<Vec<PathBuf>> {
    let metadata = fs::symlink_metadata(source)
        .with_context(|| format!("cannot inspect simulation source {}", source.display()))?;
    ensure!(
        !metadata.file_type().is_symlink(),
        "simulation source must not be a symlink: {}",
        source.display()
    );
    ensure!(
        metadata.is_file() || metadata.is_dir(),
        "simulation source must be a regular media file or a directory"
    );
    let source = fs::canonicalize(source)
        .with_context(|| format!("cannot resolve simulation source {}", source.display()))?;
    if metadata.is_file() {
        ensure!(
            supported(&source),
            "unsupported simulation media file {}; use a video or still image, not a playlist",
            source.display()
        );
        return Ok(vec![source]);
    }
    let mut files = Vec::new();
    for entry in fs::read_dir(&source)
        .with_context(|| format!("cannot list simulation directory {}", source.display()))?
    {
        let entry = entry.context("cannot read simulation directory entry")?;
        if entry.file_type()?.is_file() && supported(&entry.path()) {
            files.push(entry.path());
        }
    }
    files.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    ensure!(
        !files.is_empty(),
        "simulation directory {} contains no supported regular video or still image files at its top level",
        source.display()
    );
    Ok(files)
}

struct Manifest(PathBuf);
impl Drop for Manifest {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
impl Manifest {
    fn images(files: &[PathBuf], fps: f64) -> Result<Self> {
        let mut random = [0u8; 16];
        getrandom::fill(&mut random)
            .map_err(|e| anyhow::anyhow!("cannot generate simulation manifest name: {e}"))?;
        let nonce = random
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        let manifest =
            Self(std::env::temp_dir().join(format!("capturefab-replay-{nonce}.ffconcat")));
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut output = options
            .open(&manifest.0)
            .context("cannot create simulation image manifest")?;
        writeln!(output, "ffconcat version 1.0")?;
        for path in files.iter().chain(files.last()) {
            regular_file(path)?;
            let path = path
                .to_str()
                .context("simulation image path is not valid UTF-8")?;
            ensure!(
                !path.chars().any(char::is_control),
                "simulation image paths must not contain control characters"
            );
            // FFmpeg's concat syntax permits quoted filenames with an escaped
            // apostrophe outside the quote. Image2 pattern expansion is disabled.
            writeln!(output, "file '{}'", path.replace('\'', "'\\''"))?;
            writeln!(output, "option pattern_type none")?;
            writeln!(output, "option framerate {fps}")?;
            // A supported still extension cannot disguise a playlist that
            // causes the concat demuxer to visit other files or directories.
            writeln!(
                output,
                "option format_whitelist image2,png_pipe,jpeg_pipe,bmp_pipe,tiff_pipe,webp_pipe,ppm_pipe,pgm_pipe,pbm_pipe"
            )?;
            writeln!(output, "option protocol_whitelist file,pipe")?;
            writeln!(output, "duration {:.12}", 1.0 / fps)?;
        }
        // The repeated final input supplies the final timestamp. The decoder's
        // output cap ensures it never appears as an additional delivered frame.
        Ok(manifest)
    }
}

fn validate_format(width: u32, height: u32, fps: f64) -> Result<()> {
    ensure!(
        (64..=1920).contains(&width) && (64..=1080).contains(&height),
        "simulation output dimensions must be within 64..1920 by 64..1080"
    );
    ensure!(
        fps.is_finite() && (1.0..=120.0).contains(&fps),
        "simulation frame rate must be a finite number within 1..120 Hz"
    );
    Ok(())
}

pub fn open(
    source: &Path,
    width: u32,
    height: u32,
    fps: f64,
    timeout: Duration,
) -> Result<Box<dyn Backend>> {
    validate_format(width, height, fps)?;
    ensure!(
        !timeout.is_zero() && Instant::now().checked_add(timeout).is_some(),
        "simulation timeout must be positive and fit in the monotonic clock"
    );
    let files = source_files(source)?;
    let sequence = files
        .iter()
        .all(|path| is_image(path) && extension(path) == extension(&files[0]));
    let now = Instant::now();
    let mut backend = Replay {
        files,
        sequence,
        manifest: None,
        decoder: None,
        first: None,
        still: None,
        cursor: 0,
        clip_frames: 0,
        width,
        height,
        fps,
        timeout,
        running: false,
        next: now,
        epoch: now,
        frame_id: 0,
        timestamp_ns: 0,
    };
    backend.launch(timeout)?;
    if backend.still.is_none() {
        backend.first = Some(
            backend
                .decoded(timeout)?
                .context("simulation media contains no decodable video frames")?,
        );
    }
    Ok(Box::new(backend))
}

struct Replay {
    files: Vec<PathBuf>,
    sequence: bool,
    manifest: Option<Manifest>,
    decoder: Option<ReplayDecoder>,
    first: Option<Frame>,
    still: Option<Frame>,
    cursor: usize,
    clip_frames: u64,
    width: u32,
    height: u32,
    fps: f64,
    timeout: Duration,
    running: bool,
    next: Instant,
    epoch: Instant,
    frame_id: u64,
    timestamp_ns: u64,
}
impl Replay {
    fn clear_decoder(&mut self) {
        self.decoder = None;
        self.manifest = None;
        self.first = None;
        self.still = None;
        self.clip_frames = 0;
    }
    fn launch(&mut self, timeout: Duration) -> Result<()> {
        self.clear_decoder();
        let decoder = if self.sequence {
            let manifest = Manifest::images(&self.files, self.fps)?;
            let decoder = ReplayDecoder::sequence(
                &manifest.0,
                self.width,
                self.height,
                self.fps,
                self.files.len(),
                timeout,
            )?;
            self.manifest = Some(manifest);
            decoder
        } else {
            let path = &self.files[self.cursor];
            regular_file(path)?;
            ReplayDecoder::open(
                path,
                self.width,
                self.height,
                self.fps,
                is_image(path),
                timeout,
            )?
        };
        self.decoder = Some(decoder);
        if self.files.len() == 1 && is_image(&self.files[0]) {
            self.still = Some(
                self.decoded(timeout)?
                    .context("simulation image contains no decodable frame")?,
            );
            // A static test image needs one bounded frame allocation, rather
            // than a fresh FFmpeg process on every camera frame.
            self.decoder = None;
            self.manifest = None;
        }
        Ok(())
    }
    fn decoded(&mut self, timeout: Duration) -> Result<Option<Frame>> {
        let frame = self
            .decoder
            .as_mut()
            .context("simulation decoder is stopped")?
            .next(timeout)?;
        if frame.is_some() {
            self.clip_frames += 1;
        }
        Ok(frame)
    }
}
impl RegisterIo for Replay {
    fn read_memory(&mut self, address: u64, length: usize) -> Result<Vec<u8>> {
        let mut memory = [0u8; 0x134];
        memory[0x100..0x104].copy_from_slice(&self.width.to_le_bytes());
        memory[0x104..0x108].copy_from_slice(&self.height.to_le_bytes());
        memory[0x108..0x10c].copy_from_slice(&RGB8.to_le_bytes());
        memory[0x120..0x128].copy_from_slice(&self.fps.to_le_bytes());
        memory[0x128..0x12c].copy_from_slice(&(self.width * self.height * 3).to_le_bytes());
        memory[0x130..0x134].copy_from_slice(&(u32::from(self.running)).to_le_bytes());
        let start = usize::try_from(address)?;
        let end = start
            .checked_add(length)
            .context("simulation register address overflow")?;
        Ok(memory
            .get(start..end)
            .context("invalid simulation register address")?
            .to_vec())
    }
    fn write_memory(&mut self, address: u64, data: &[u8]) -> Result<()> {
        if address == 0x130 && data.len() == 4 {
            ensure!(
                u32::from_le_bytes(data.try_into()?) <= 1,
                "simulation acquisition control must be 0 or 1"
            );
            // Backend::start/stop owns the decoder lifecycle. The command is
            // acknowledged after the Camera has configured those resources.
            return Ok(());
        }
        ensure!(
            !self.running,
            "stop acquisition before changing replay format or frame rate"
        );
        let (mut width, mut height, mut fps) = (self.width, self.height, self.fps);
        match (address, data.len()) {
            (0x100, 4) => width = u32::from_le_bytes(data.try_into()?),
            (0x104, 4) => height = u32::from_le_bytes(data.try_into()?),
            (0x120, 8) => fps = f64::from_le_bytes(data.try_into()?),
            _ => bail!("simulation register is read-only or invalid"),
        }
        validate_format(width, height, fps)?;
        if (width, height, fps) != (self.width, self.height, self.fps) {
            self.clear_decoder();
            (self.width, self.height, self.fps, self.cursor) = (width, height, fps, 0);
        }
        Ok(())
    }
}
impl Backend for Replay {
    fn xml(&mut self) -> Result<String> {
        Ok(XML.into())
    }
    fn start(&mut self, _: usize) -> Result<()> {
        if self.decoder.is_none() && self.still.is_none() {
            self.cursor = 0;
            self.launch(self.timeout)?;
        }
        self.running = true;
        self.next = Instant::now();
        Ok(())
    }
    fn stop(&mut self) -> Result<()> {
        self.running = false;
        self.clear_decoder();
        Ok(())
    }
    fn next_frame(&mut self, timeout: Duration) -> Result<Frame> {
        ensure!(self.running, "camera is not streaming");
        let deadline = Instant::now()
            .checked_add(timeout)
            .context("simulation frame timeout exceeds the monotonic clock")?;
        let scheduled = self.next.max(Instant::now());
        let wait = scheduled.saturating_duration_since(Instant::now());
        if wait > timeout {
            // A producer polling with a shorter timeout than the frame period
            // must still block for that timeout instead of spinning hot.
            thread::sleep(timeout);
            bail!("simulation frame capture timed out before the next scheduled frame");
        }
        if !wait.is_zero() {
            thread::sleep(wait);
        }
        let mut frame = loop {
            if let Some(first) = self.first.take() {
                break first;
            }
            if let Some(still) = &self.still {
                break still.clone();
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            ensure!(!remaining.is_zero(), "simulation frame capture timed out");
            if let Some(frame) = self.decoded(remaining)? {
                break frame;
            }
            ensure!(
                self.clip_frames > 0,
                "simulation media contains no decodable video frames: {}",
                self.files[self.cursor].display()
            );
            if self.sequence {
                ensure!(
                    self.clip_frames == self.files.len() as u64,
                    "simulation image sequence ended after {} frames, expected {}",
                    self.clip_frames,
                    self.files.len()
                );
            } else {
                self.cursor = (self.cursor + 1) % self.files.len();
            }
            self.launch(deadline.saturating_duration_since(Instant::now()))?;
        };
        ensure!(
            (frame.width, frame.height, frame.pixel_format) == (self.width, self.height, RGB8),
            "simulation decoder returned an unexpected frame format"
        );
        self.frame_id += 1;
        self.timestamp_ns = (self.epoch.elapsed().as_nanos().min(u64::MAX as u128) as u64)
            .max(self.timestamp_ns.saturating_add(1));
        frame.id = self.frame_id;
        frame.timestamp_ns = self.timestamp_ns;
        // Never catch up in a burst after a slow consumer or clip transition.
        let period = Duration::from_secs_f64(1.0 / self.fps);
        let now = Instant::now();
        self.next = if now >= scheduled + period {
            now + period
        } else {
            scheduled + period
        };
        Ok(frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;

    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            let mut nonce = [0u8; 16];
            getrandom::fill(&mut nonce).unwrap();
            let suffix = nonce.iter().map(|b| format!("{b:02x}")).collect::<String>();
            let path = std::env::temp_dir().join(format!("capturefab-replay-test-{suffix}"));
            fs::create_dir(&path).unwrap();
            Self(fs::canonicalize(path).unwrap())
        }
        fn file(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn ppm(path: &Path, color: [u8; 3]) {
        let mut bytes = b"P6\n16 16\n255\n".to_vec();
        for _ in 0..16 * 16 {
            bytes.extend(color);
        }
        fs::write(path, bytes).unwrap();
    }
    fn png(path: &Path, width: u32, height: u32, color: [u8; 3]) {
        let mut encoder = png::Encoder::new(fs::File::create(path).unwrap(), width, height);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer
            .write_image_data(&color.repeat((width * height) as usize))
            .unwrap();
    }
    fn ffmpeg() -> Option<std::sync::Arc<crate::media::Executable>> {
        let executable = crate::media::executable().ok()?;
        if crate::media::command(&executable)
            .arg("-version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .ok()?
            .success()
        {
            Some(executable)
        } else {
            None
        }
    }
    fn video(path: &Path, colors: &[[u8; 3]], fps: u32) {
        let exe = ffmpeg().expect("FFmpeg fixture generator");
        let mut child = crate::media::command(&exe)
            .args([
                "-nostdin",
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "rawvideo",
                "-pixel_format",
                "rgb24",
                "-video_size",
                "16x16",
                "-framerate",
                &fps.to_string(),
                "-i",
                "pipe:0",
                "-c:v",
                "ffv1",
            ])
            .arg(path)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        for color in colors {
            for _ in 0..16 * 16 {
                input.write_all(color).unwrap();
            }
        }
        drop(input);
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    fn color(frame: &Frame) -> [u8; 3] {
        frame.data[frame.data.len() / 2..][..3].try_into().unwrap()
    }
    fn frames(camera: &mut dyn Backend, expected: &[[u8; 3]]) {
        let mut id = 0;
        let mut timestamp = 0;
        for expected in expected {
            let frame = camera.next_frame(Duration::from_secs(3)).unwrap();
            assert_eq!(color(&frame), *expected);
            assert_eq!(
                (frame.width, frame.height, frame.pixel_format),
                (64, 64, RGB8)
            );
            assert_eq!(frame.data.len(), 64 * 64 * 3);
            assert!(frame.id > id);
            assert!(frame.timestamp_ns > timestamp);
            (id, timestamp) = (frame.id, frame.timestamp_ns);
        }
    }

    #[test]
    fn listing_is_lexicographic_single_level_and_ignores_unrelated_files() {
        let dir = Directory::new();
        for name in ["2.mp4", "10.MKV", "01.png", "notes.txt", "playlist.m3u8"] {
            fs::write(dir.file(name), b"fixture").unwrap();
        }
        fs::create_dir(dir.file("nested")).unwrap();
        fs::write(dir.file("nested/00.mp4"), b"fixture").unwrap();
        let names = source_files(&dir.0)
            .unwrap()
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(names, ["01.png", "10.MKV", "2.mp4"]);
        assert_eq!(
            source_files(&dir.file("2.mp4")).unwrap(),
            [dir.file("2.mp4")]
        );
        assert!(source_files(&dir.file("playlist.m3u8")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn listing_never_follows_symlink_files_or_directories() {
        use std::os::unix::fs::symlink;
        let dir = Directory::new();
        let outside = Directory::new();
        fs::write(outside.file("outside.mp4"), b"fixture").unwrap();
        fs::write(dir.file("real.mp4"), b"fixture").unwrap();
        symlink(outside.file("outside.mp4"), dir.file("a.mp4")).unwrap();
        symlink(&outside.0, dir.file("nested")).unwrap();
        symlink(outside.file("missing.mp4"), dir.file("broken.mp4")).unwrap();
        assert_eq!(source_files(&dir.0).unwrap(), [dir.file("real.mp4")]);
        assert!(source_files(&dir.file("a.mp4")).is_err());
        assert!(source_files(&dir.file("nested")).is_err());
    }

    #[test]
    fn missing_and_empty_sources_have_actionable_errors() {
        let dir = Directory::new();
        assert!(
            source_files(&dir.0)
                .unwrap_err()
                .to_string()
                .contains("no supported regular")
        );
        assert!(
            source_files(&dir.file("missing.mp4"))
                .unwrap_err()
                .to_string()
                .contains("cannot inspect simulation source")
        );
        assert!(open(&dir.0, 64, 64, f64::NAN, Duration::from_secs(1)).is_err());
    }

    #[test]
    fn still_frames_loop_once_per_filename_with_literal_special_names() {
        if ffmpeg().is_none() {
            return;
        }
        let dir = Directory::new();
        let red = [255, 0, 0];
        let blue = [0, 0, 255];
        ppm(&dir.file("02 second%.ppm"), blue);
        ppm(&dir.file("01 'first'%d.ppm"), red);
        let mut camera = open(&dir.0, 64, 64, 120.0, Duration::from_secs(3)).unwrap();
        camera.start(64 * 64 * 3).unwrap();
        frames(&mut *camera, &[red, blue, red, blue, red, blue]);
        camera.stop().unwrap();
        assert!(camera.next_frame(Duration::from_secs(1)).is_err());
    }

    #[test]
    fn still_sequences_preserve_order_when_image_dimensions_change() {
        if ffmpeg().is_none() {
            return;
        }
        let dir = Directory::new();
        let red = [255, 0, 0];
        let blue = [0, 0, 255];
        png(&dir.file("01.png"), 16, 16, red);
        png(&dir.file("02.png"), 32, 16, blue);
        let mut camera = open(&dir.0, 64, 64, 30.0, Duration::from_secs(3)).unwrap();
        camera.start(64 * 64 * 3).unwrap();
        frames(&mut *camera, &[red, blue, red, blue]);
    }

    #[test]
    fn different_video_clips_loop_in_filename_order_with_monotonic_metadata() {
        if ffmpeg().is_none() {
            return;
        }
        let dir = Directory::new();
        let red = [255, 0, 0];
        let blue = [0, 0, 255];
        video(&dir.file("b.mkv"), &[blue, blue], 20);
        video(&dir.file("a.mkv"), &[red, red], 20);
        let mut camera = open(&dir.0, 64, 64, 20.0, Duration::from_secs(3)).unwrap();
        camera.start(64 * 64 * 3).unwrap();
        frames(&mut *camera, &[red, red, blue, blue, red, red, blue, blue]);
        camera.stop().unwrap();
    }

    #[test]
    fn one_video_loops_and_resamples_to_the_requested_frame_rate() {
        if ffmpeg().is_none() {
            return;
        }
        let dir = Directory::new();
        let red = [255, 0, 0];
        let blue = [0, 0, 255];
        video(&dir.file("clip.mkv"), &[red, blue], 10);
        let mut camera = open(&dir.file("clip.mkv"), 64, 64, 20.0, Duration::from_secs(3)).unwrap();
        camera.start(64 * 64 * 3).unwrap();
        frames(&mut *camera, &[red, red, blue, blue, red, red, blue, blue]);
    }

    #[test]
    fn corrupt_selected_media_fails_instead_of_being_skipped() {
        if ffmpeg().is_none() {
            return;
        }
        let dir = Directory::new();
        fs::write(dir.file("bad.mp4"), b"not a video").unwrap();
        let error = match open(&dir.file("bad.mp4"), 64, 64, 30.0, Duration::from_secs(3)) {
            Ok(_) => panic!("corrupt media opened"),
            Err(error) => error,
        };
        assert!(format!("{error:#}").contains("cannot decode simulation media"));
        fs::write(dir.file("bad.mp4"), b"").unwrap();
        assert!(open(&dir.file("bad.mp4"), 64, 64, 30.0, Duration::from_secs(3)).is_err());
    }

    #[test]
    fn format_controls_validate_changes_and_pacing_does_not_burst_after_stalls() {
        if ffmpeg().is_none() {
            return;
        }
        let dir = Directory::new();
        ppm(&dir.file("frame.ppm"), [255, 0, 0]);
        let mut camera = open(&dir.0, 64, 64, 30.0, Duration::from_secs(3)).unwrap();
        let rate = camera.read_memory(0x120, 8).unwrap();
        assert!(camera.write_memory(0x120, &f64::NAN.to_le_bytes()).is_err());
        assert_eq!(camera.read_memory(0x120, 8).unwrap(), rate);
        camera.write_memory(0x100, &128u32.to_le_bytes()).unwrap();
        camera.write_memory(0x120, &10f64.to_le_bytes()).unwrap();
        camera.start(128 * 64 * 3).unwrap();
        assert!(camera.write_memory(0x104, &128u32.to_le_bytes()).is_err());
        let frame = camera.next_frame(Duration::from_secs(3)).unwrap();
        assert_eq!(
            (frame.width, frame.height, frame.data.len()),
            (128, 64, 128 * 64 * 3)
        );
        let short_poll = Instant::now();
        assert!(camera.next_frame(Duration::from_millis(5)).is_err());
        assert!(short_poll.elapsed() >= Duration::from_millis(4));
        thread::sleep(Duration::from_millis(150));
        camera.next_frame(Duration::from_secs(3)).unwrap();
        assert!(camera.next_frame(Duration::from_millis(5)).is_err());
        camera.stop().unwrap();
        camera.start(128 * 64 * 3).unwrap();
        assert!(camera.next_frame(Duration::from_secs(3)).unwrap().id > frame.id);
    }
}
