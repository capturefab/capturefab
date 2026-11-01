//! Frame inspections in a camera worker. The camera thread picks the frame,
//! from a stream as it arrives or by acquiring one from a stopped camera,
//! and hands it, shared and uncopied, to an inspector thread that measures
//! it, writes its preview to a file and replies: acquisition never waits for
//! that work.
use super::*;
use crate::inspect::PreviewFormat;
use std::{
    fs::{self, OpenOptions},
    io::{self, Write as _},
    sync::{atomic::AtomicUsize, mpsc::SyncSender},
    time::{SystemTime, UNIX_EPOCH},
};

/// Inspections a worker measures or holds at once: each may keep a
/// full-resolution frame and its conversions.
pub(crate) const MAX_INSPECTIONS: usize = 8;
/// The most previews the private folder keeps, and their total size.
const MAX_IMAGES: usize = 64;
const MAX_IMAGE_BYTES: u64 = 64 * 1024 * 1024;
static NEXT_IMAGE: AtomicU64 = AtomicU64::new(1);

/// Where previews go unless a request names a file: private, bounded, and
/// shown by doctor, so a person can allow reading it once. Without a
/// runtime folder, this user's temporary folder holds them.
pub(crate) fn preview_dir() -> Result<PathBuf> {
    crate::dirs::scratch()
        .map(|dir| dir.join("inspect"))
        .map_err(|error| {
            let mut refused = crate::types::Categorized::new(
                crate::types::ErrorKind::PermissionDenied,
                format!("no folder for the preview: {error:#}"),
            );
            refused.hint = Some(
                "write it to a file with --preview FILE, or skip it with --preview none".into(),
            );
            refused.into()
        })
}

/// What a `frame` request asks for.
pub(super) struct Request {
    fresh: u32,
    /// The frame wait asked for; 0 for the camera's default.
    timeout_ms: u64,
    timeout: Duration,
    warmup_ms: Option<u64>,
    max_dimension: u32,
    format: PreviewFormat,
    region: Option<[f64; 4]>,
    /// The file the preview goes to; the private folder when unset.
    preview: Option<PathBuf>,
}
impl Request {
    pub(super) fn of(command: &WorkerCommand) -> Option<Self> {
        let Some(SessionCommand::Frame {
            fresh,
            timeout_ms,
            max_dimension,
            format,
            region,
            preview,
            warmup_ms,
        }) = command.session()
        else {
            return None;
        };
        Some(Self {
            fresh: *fresh,
            timeout_ms: *timeout_ms,
            timeout: Duration::from_millis(*timeout_ms),
            warmup_ms: *warmup_ms,
            max_dimension: *max_dimension,
            format: *format,
            region: *region,
            preview: preview.as_ref().map(PathBuf::from),
        })
    }
}

/// A frame request waiting for its frame from a streaming camera.
pub(super) struct Waiting {
    id: Option<u64>,
    cancel: Arc<AtomicBool>,
    request: Request,
    /// Frames still to let pass.
    left: u32,
    waited: u32,
    /// Frames were let pass because they were exposed before the camera's
    /// last settings change.
    after_change: bool,
    /// The camera's counters when the request began.
    baseline: Baseline,
    deadline: Instant,
    reply: Answer,
}

/// A frame for the inspector thread, and what its reply says of it.
struct Job {
    frame: Arc<Frame>,
    received: Instant,
    camera: crate::types::CameraInfo,
    request: Request,
    /// `live` for a frame from a stream, `grab` for one acquired for this.
    source: &'static str,
    waited: u32,
    after_change: bool,
    warmup: Option<Warmup>,
    /// Frames the transport lost and that arrived incomplete while the
    /// frame was awaited, and packets resent, where it counts them.
    lost: Option<(u64, u64, u64)>,
    notes: Vec<String>,
    reply: Answer,
}

impl Job {
    fn into_reply(self) -> Answer {
        self.reply
    }
}

/// The inspector thread, started with its first inspection.
#[derive(Default)]
pub(super) struct Inspector {
    jobs: Option<SyncSender<Job>>,
    held: Arc<AtomicUsize>,
    /// Bytes of the frames held, which with their conversions must fit
    /// the memory this process may use.
    bytes: Arc<AtomicUsize>,
}
impl Inspector {
    /// Hand `job` to the inspector thread, or refuse it at once when it
    /// holds as many as it may: `MAX_INSPECTIONS`, and frames of at most a
    /// quarter of the memory left. One is always taken.
    fn inspect(&mut self, job: Job) {
        let size = job.frame.data.len();
        let held = self.held.fetch_add(1, Ordering::AcqRel);
        let bytes = self.bytes.fetch_add(size, Ordering::AcqRel);
        let room = crate::host::memory_budget().map_or(usize::MAX, |budget| {
            usize::try_from(budget / 4).unwrap_or(usize::MAX)
        });
        if held >= MAX_INSPECTIONS || (held > 0 && bytes.saturating_add(size) > room) {
            self.held.fetch_sub(1, Ordering::AcqRel);
            self.bytes.fetch_sub(size, Ordering::AcqRel);
            (job.reply)(Err(crate::types::Categorized::new(
                crate::types::ErrorKind::CameraBusy,
                "too many frame inspections are waiting; retry when one returns",
            )
            .into()));
            return;
        }
        let jobs = self.jobs.get_or_insert_with(|| {
            let (jobs, inbox) = mpsc::sync_channel::<Job>(MAX_INSPECTIONS);
            let held = self.held.clone();
            let bytes = self.bytes.clone();
            std::thread::Builder::new()
                .name("capturefab-inspector".into())
                .spawn(move || {
                    for job in inbox {
                        let result = measure(&job);
                        let size = job.frame.data.len();
                        // The frame is let go before the reply is sent.
                        let reply = job.into_reply();
                        held.fetch_sub(1, Ordering::AcqRel);
                        bytes.fetch_sub(size, Ordering::AcqRel);
                        reply(result);
                    }
                })
                .expect("spawn frame inspector");
            jobs
        });
        if let Err(mpsc::TrySendError::Full(job) | mpsc::TrySendError::Disconnected(job)) =
            jobs.try_send(job)
        {
            self.held.fetch_sub(1, Ordering::AcqRel);
            self.bytes.fetch_sub(size, Ordering::AcqRel);
            (job.reply)(Err(anyhow::anyhow!("the frame inspector stopped")));
        }
    }
}

/// Take up frame request `envelope`. A streaming camera's next frames serve
/// it as they arrive; a stopped camera acquires them now, as many as it
/// asks to let pass and one more, and stops again unless it lingers.
#[allow(clippy::too_many_arguments)]
pub(super) fn begin(
    h: &WorkerHandle,
    camera: &mut Option<Camera>,
    auto: &mut Option<AutoController>,
    envelope: Envelope,
    mut request: Request,
    inspector: &mut Inspector,
    waiting: &mut Vec<Waiting>,
    stream: &mut super::JobStream,
) {
    let Some(c) = camera.as_mut() else {
        (envelope.reply)(Err(no_camera()));
        return;
    };
    request.timeout = frame_wait(c, request.timeout_ms);
    let deadline = deadline_after(request.timeout);
    if c.is_streaming() {
        let cancel = h
            .running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|running| running.cancel.clone())
            .unwrap_or_default();
        if let Some(id) = envelope.id {
            h.deferred
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(id, cancel.clone());
        }
        // A camera sending frames only when triggered sends one now.
        if let Err(error) = c.request_frame() {
            finish(h, envelope.id, envelope.reply, Err(error));
            return;
        }
        // A camera that lingers goes on streaming until this is served.
        super::used(stream, c);
        waiting.push(Waiting {
            id: envelope.id,
            cancel,
            left: request.fresh.saturating_sub(1),
            waited: 0,
            after_change: false,
            baseline: Baseline::now(h),
            deadline,
            request,
            reply: envelope.reply,
        });
        return;
    }
    let info = c.info.clone();
    let count = request.fresh.max(1);
    let baseline = Baseline::now(h);
    match acquire(
        h,
        camera,
        auto,
        count,
        deadline,
        request.timeout,
        request.warmup_ms,
        Some(stream),
    ) {
        Ok((frame, received, settled, warmup)) => inspector.inspect(Job {
            frame: Arc::new(frame),
            received,
            camera: info,
            source: "grab",
            waited: count,
            after_change: false,
            warmup,
            lost: losses(&baseline, h),
            notes: if settled {
                Vec::new()
            } else {
                vec!["auto exposure had not settled when this frame was taken".into()]
            },
            request,
            reply: envelope.reply,
        }),
        Err(error) => (envelope.reply)(Err(error)),
    }
}

/// Acquire `count` frames from a stopped camera, warming up one that sets
/// its own exposure and settling Capturefab's auto exposure first, and stop
/// it again, or with `stream` leave a camera that lingers streaming for a
/// moment: the last frame, when it came, whether auto exposure had settled,
/// and the warm-up.
#[allow(clippy::too_many_arguments)]
pub(super) fn acquire(
    h: &WorkerHandle,
    camera: &mut Option<Camera>,
    auto: &mut Option<AutoController>,
    count: u32,
    deadline: Instant,
    wait: Duration,
    warmup_ms: Option<u64>,
    stream: Option<&mut super::JobStream>,
) -> Result<(Frame, Instant, bool, Option<Warmup>)> {
    let c = camera_mut(camera)?;
    let was_streaming = c.is_streaming();
    c.start()?;
    let result = (|| -> Result<(Frame, Instant, bool, Option<Warmup>)> {
        let warmup = if was_streaming {
            None
        } else {
            warm_up(h, c, auto, warmup_ms)?
        };
        // A software-triggered camera sends no frames to meter.
        let settled = was_streaming
            || auto.is_none()
            || (!c.software_triggered()
                && settle_auto(
                    h,
                    c,
                    auto,
                    deadline.saturating_duration_since(Instant::now()),
                )?);
        let mut last = None;
        for _ in 0..count {
            if Instant::now() >= deadline {
                return Err(no_new_frame(&c.info.id, wait));
            }
            c.request_frame()?;
            match next_frame(h, c, auto, deadline)? {
                Some(frame) => {
                    h.count_frame(&frame);
                    last = Some((frame, Instant::now()));
                }
                None => return Err(missing_frame(c, wait)),
            }
        }
        let (frame, received) = last.context("no frame was asked for")?;
        Ok((frame, received, settled, warmup))
    })();
    let lingers = match (stream, super::linger(c)) {
        (Some(stream), Some(linger)) if !was_streaming && result.is_ok() => {
            stream.started = true;
            stream.linger_until = Some(Instant::now() + linger);
            true
        }
        _ => false,
    };
    let stopped = if !was_streaming && !lingers {
        c.stop()
    } else {
        Ok(())
    };
    refresh(h, camera);
    let acquired = result?;
    stopped?;
    Ok(acquired)
}

/// Serve the waiting requests a streaming camera's new `frame` is due for:
/// each takes it, shared, once it has let its frames pass. The frame is
/// handed back for the recording when `keep`, copied only if an inspection
/// still holds it.
pub(super) fn serve(
    c: &mut Camera,
    frame: Frame,
    keep: bool,
    waiting: &mut Vec<Waiting>,
    inspector: &mut Inspector,
    h: &WorkerHandle,
) -> Option<Frame> {
    if waiting.is_empty() {
        return keep.then_some(frame);
    }
    let received = Instant::now();
    // A frame exposed before the camera's last settings change counts for
    // none of them.
    let stale = c.frame_predates_change();
    let mut due = Vec::new();
    for item in std::mem::take(waiting) {
        if item.left == 0 && !stale {
            due.push(item);
            continue;
        }
        let mut item = item;
        if stale {
            item.after_change = true;
        } else {
            item.left -= 1;
        }
        item.waited += 1;
        let _ = c.request_frame();
        waiting.push(item);
    }
    if due.is_empty() {
        return keep.then_some(frame);
    }
    let shared = Arc::new(frame);
    for item in due {
        forget(h, item.id);
        inspector.inspect(Job {
            frame: shared.clone(),
            received,
            camera: c.info.clone(),
            request: item.request,
            source: "live",
            waited: item.waited + 1,
            after_change: item.after_change,
            warmup: None,
            lost: losses(&item.baseline, h),
            notes: Vec::new(),
            reply: item.reply,
        });
    }
    keep.then(|| Arc::try_unwrap(shared).unwrap_or_else(|shared| (*shared).clone()))
}

/// End the waiting requests that were cancelled, whose deadline passed, or
/// whose camera stopped streaming; with `gone`, every one, for that reason.
pub(super) fn expire(
    h: &WorkerHandle,
    camera: &mut Option<Camera>,
    waiting: &mut Vec<Waiting>,
    gone: Option<&dyn Fn() -> anyhow::Error>,
) {
    if waiting.is_empty() {
        return;
    }
    let now = Instant::now();
    let streaming = camera.as_ref().is_some_and(Camera::is_streaming);
    for item in std::mem::take(waiting) {
        let error = if let Some(gone) = gone {
            gone()
        } else if item.cancel.load(Ordering::Acquire) {
            interrupted("inspection cancelled")
        } else if now >= item.deadline {
            match camera.as_mut() {
                Some(c) => missing_frame(c, item.request.timeout),
                None => no_camera(),
            }
        } else if !streaming {
            match camera.as_ref() {
                Some(c) => anyhow::anyhow!(
                    "{} stopped streaming{}",
                    crate::media::redact_url(&c.info.id),
                    h.snapshot()
                        .last_error
                        .map(|error| format!(": {error}"))
                        .unwrap_or_default()
                ),
                None => crate::types::Categorized::error(
                    crate::types::ErrorKind::Disconnected,
                    "the camera was disconnected",
                ),
            }
        } else {
            waiting.push(item);
            continue;
        };
        forget(h, item.id);
        (item.reply)(Err(error));
    }
}

/// Frames lost and incomplete since `baseline`, and packets resent, when
/// the transport counts them.
fn losses(baseline: &Baseline, h: &WorkerHandle) -> Option<(u64, u64, u64)> {
    let (_, lost, incomplete, resent) = baseline.since(h);
    Some((lost?, incomplete?, resent?))
}

/// A request that will not be answered later: its cancel flag is let go.
fn forget(h: &WorkerHandle, id: Option<u64>) {
    if let Some(id) = id {
        h.deferred
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
    }
}
fn finish(h: &WorkerHandle, id: Option<u64>, reply: Answer, result: Result<Value>) {
    forget(h, id);
    reply(result);
}
fn no_camera() -> anyhow::Error {
    anyhow::anyhow!("no camera connected; select --camera or use connect in a session")
}

/// Why no frame came in time: a trigger that never fired, with its
/// configuration, else that no new frame came.
fn missing_frame(c: &mut Camera, wait: Duration) -> anyhow::Error {
    let error = frame_wait_error(c, wait);
    if crate::types::classify(&error).kind == crate::types::ErrorKind::TriggerTimeout {
        error
    } else {
        no_new_frame(&c.info.id, wait)
    }
}

/// An inspection without a frame; "timed out" makes the CLI exit 4.
pub(super) fn no_new_frame(camera: &str, wait: Duration) -> anyhow::Error {
    // Rounded up, so a deadline set for 3 s just before reads "3 s".
    let ms = u64::try_from(wait.as_micros().div_ceil(1000)).unwrap_or(u64::MAX);
    crate::types::frame_timeout(format!(
        "no new frame from {} within {} (timed out); a triggered camera needs TriggerSoftware",
        crate::media::redact_url(camera),
        crate::scheduling::format_duration(ms)
    ))
}

/// Measure a job's frame and write its preview: the reply.
fn measure(job: &Job) -> Result<Value> {
    let request = &job.request;
    let mut reply = measurements(
        &job.frame,
        job.camera.profile.kind,
        &job.camera.id,
        request.region,
        request.max_dimension,
        request.format,
        request.preview.as_deref(),
    )?;
    if !job.notes.is_empty() {
        reply["settled"] = json!(false);
    }
    if job.after_change {
        reply["after_change"] = json!(true);
    }
    reply["frames"] = json!(job.waited);
    if let Some((lost, incomplete, resent)) = job.lost {
        reply["dropped"] = json!(lost);
        reply["incomplete"] = json!(incomplete);
        reply["resent"] = json!(resent);
    }
    if let Some(warmup) = job.warmup {
        reply["warmup_ms"] = json!(warmup.ms);
        if !warmup.settled {
            reply["settled"] = json!(false);
            reply["warnings"] = json!([unsettled_warning(warmup.ms)]);
        }
    }
    if let Some(measured) = reply["notes"].as_array_mut() {
        measured.extend(job.notes.iter().cloned().map(Value::from));
    }
    let frame = &job.frame;
    reply["camera"] = json!(crate::media::redact_url(&job.camera.id));
    reply["frame"] = json!({
        "id": frame.id,
        "width": frame.width,
        "height": frame.height,
        "pixel_format": crate::inspect::pixel_format_label(frame.pixel_format),
        "device_ts_ns": frame.device_ts_ns,
        "wall_ms": frame.wall_ms,
        "age_ms": u64::try_from(job.received.elapsed().as_millis()).unwrap_or(u64::MAX),
        "source": job.source,
        "waited_frames": job.waited,
    });
    Ok(reply)
}

/// Measure `frame`'s `region` and write its preview, to `preview` or the
/// private folder: the reply's measurements and `image`.
pub(super) fn measurements(
    frame: &Frame,
    kind: crate::camera_profile::CameraKind,
    camera: &str,
    region: Option<[f64; 4]>,
    max_dimension: u32,
    format: PreviewFormat,
    preview: Option<&Path>,
) -> Result<Value> {
    let mut inspection = crate::inspect::inspect(frame, kind, region, max_dimension, format)?;
    let image = inspection.image.take();
    let mut reply = serde_json::to_value(&inspection)?;
    if let Some(fields) = reply.as_object_mut() {
        fields.remove("image");
    }
    if let Some(image) = image {
        let extension = if image.mime == "image/jpeg" {
            "jpg"
        } else {
            "png"
        };
        let path = match preview {
            Some(path) => {
                crate::storage::save_one(&image.bytes, path)?;
                path.to_path_buf()
            }
            None => private_image(
                &preview_dir()?,
                &safe_label(camera),
                &image.bytes,
                extension,
            )?,
        };
        reply["image"] = json!({
            "mime": image.mime,
            "width": image.width,
            "height": image.height,
            "bytes": image.bytes.len(),
            "path": path,
        });
    }
    Ok(reply)
}

/// A file name part made of a camera ID: credentials redacted first, then
/// anything but letters, digits, - and _ replaced.
fn safe_label(value: &str) -> String {
    // Redact before replacing punctuation: otherwise a URL password loses
    // its URL shape and would survive into the name.
    let value = crate::scrub::redact_urls(&crate::media::redact_url(value));
    let label: String = value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '-'
            }
        })
        .take(64)
        .collect();
    if label.is_empty() {
        "camera".into()
    } else {
        label
    }
}

/// Write `bytes` as a new private file in `dir` (0700, files 0600, never a
/// symlinked folder), then keep only the newest `MAX_IMAGES` files and
/// `MAX_IMAGE_BYTES` there.
fn private_image(dir: &Path, label: &str, bytes: &[u8], extension: &str) -> Result<PathBuf> {
    crate::dirs::ensure_private_dir(dir)?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    for _ in 0..100 {
        let seq = NEXT_IMAGE.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!(
            "{label}-{stamp}-{}-{seq}.{extension}",
            std::process::id()
        ));
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(mut file) => {
                if let Err(error) = file.write_all(bytes) {
                    let _ = fs::remove_file(&path);
                    return Err(error.into());
                }
                drop(file);
                prune_images(dir, Some(&path))?;
                return Ok(path);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err(anyhow::anyhow!("no free preview name in {}", dir.display()))
}

/// Remove all but the newest previews, never `keep`.
fn prune_images(dir: &Path, keep: Option<&Path>) -> Result<()> {
    let mut files = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let meta = fs::symlink_metadata(entry.path())?;
        if meta.is_file() && !meta.file_type().is_symlink() {
            files.push((
                entry.path(),
                meta.modified().unwrap_or(UNIX_EPOCH),
                meta.len(),
            ));
        }
    }
    files.sort_by(|a, b| {
        (keep == Some(b.0.as_path()))
            .cmp(&(keep == Some(a.0.as_path())))
            .then_with(|| b.1.cmp(&a.1))
            .then_with(|| b.0.cmp(&a.0))
    });
    let mut bytes = 0u64;
    for (index, (path, _, size)) in files.into_iter().enumerate() {
        bytes = bytes.saturating_add(size);
        if index >= MAX_IMAGES || bytes > MAX_IMAGE_BYTES {
            match fs::remove_file(path) {
                Ok(()) => (),
                Err(error) if error.kind() == io::ErrorKind::NotFound => (),
                Err(error) => return Err(error.into()),
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_images_are_bounded_private_and_never_follow_symlinks() {
        let root = std::env::temp_dir().join(format!(
            "capturefab-inspect-images-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        for index in 0..MAX_IMAGES + 3 {
            private_image(&root, &format!("camera-{index}"), b"jpeg", "jpg").unwrap();
        }
        assert_eq!(fs::read_dir(&root).unwrap().count(), MAX_IMAGES);
        // Sparse files exercise the byte limit without consuming 80 MiB
        // on the shared build disk.
        for index in 0..2 {
            let path = private_image(&root, &format!("large-{index}"), b"jpeg", "jpg").unwrap();
            OpenOptions::new()
                .write(true)
                .open(path)
                .unwrap()
                .set_len(40 * 1024 * 1024)
                .unwrap();
        }
        prune_images(&root, None).unwrap();
        let size: u64 = fs::read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().metadata().unwrap().len())
            .sum();
        assert!(size <= MAX_IMAGE_BYTES);
        #[cfg(unix)]
        {
            use std::os::unix::fs::{PermissionsExt, symlink};
            assert_eq!(
                fs::metadata(&root).unwrap().permissions().mode() & 0o777,
                0o700
            );
            for file in fs::read_dir(&root).unwrap() {
                assert_eq!(
                    file.unwrap().metadata().unwrap().permissions().mode() & 0o777,
                    0o600
                );
            }
            let link = root.with_extension("link");
            symlink(&root, &link).unwrap();
            assert!(private_image(&link, "camera", b"jpeg", "jpg").is_err());
            fs::remove_file(link).unwrap();
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn preview_names_carry_no_credentials() {
        for camera in [
            "rtsp://alice:private-password@host/path",
            "rtsp://host/path?password=private-password",
        ] {
            let label = safe_label(camera);
            assert!(!label.contains("private-password"), "{label}");
            assert!(
                !label.contains("alice") || camera.contains("alice"),
                "{label}"
            );
        }
        assert_eq!(safe_label(""), "camera");
    }

    #[test]
    fn missing_frames_say_they_timed_out_without_credentials() {
        let error = no_new_frame("rtsp://user:secret@cam/live", Duration::from_millis(3000));
        assert_eq!(
            error.to_string(),
            "no new frame from rtsp://<redacted>@cam/live within 3 s (timed out); a triggered camera needs TriggerSoftware"
        );
        assert_eq!(crate::cli::error_code(&error), ("frame_timeout", 4));
    }
}
