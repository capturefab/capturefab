//! Renderer frame sequences for the website's demonstration clips: frames
//! are written off the UI thread at fast PNG compression, and each frame's
//! staging time goes to a `<stem>-timing.jsonl` sidecar, so the encoder can
//! time the clip by what was on screen rather than when a file finished.
use std::fs::File;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;

use iced::window;

/// Frames queued for the writers beyond those they are writing. Each holds
/// a full RGBA readback, so this bounds memory at a few frames.
const QUEUE: usize = 3;
/// PNG writer threads.
const WRITERS: usize = 2;

/// A frame read back from the renderer: its index, pixels and file.
pub(in crate::gui) type Job = (u32, window::Screenshot, PathBuf);

/// The non-`Copy` state of a renderer frame sequence.
pub(in crate::gui) struct VideoSink {
    /// Timing lines, written unbuffered in index order on the UI thread.
    timing: File,
    jobs: Option<SyncSender<Job>>,
    /// The writers' side of `jobs`, kept here too so a sink without
    /// writers still queues rather than reporting them stopped.
    _queue: Arc<Mutex<Receiver<Job>>>,
    results: Receiver<Result<(), String>>,
    writers: Vec<JoinHandle<()>>,
    /// A frame the full queue refused, retried each tick.
    pub(in crate::gui) backlog: Option<Job>,
    /// The last frame was handed over; waiting for the writers.
    pub(in crate::gui) draining: bool,
    written: u32,
    /// When the frame being taken was staged, and the draw count then.
    pub(in crate::gui) staged_at: Option<Instant>,
    pub(in crate::gui) staged_draws: u64,
    /// When the first frame was staged: the sidecar's time origin.
    pub(in crate::gui) first_staged: Option<Instant>,
    /// The preview frame shown, and each camera's frame count, at staging.
    pub(in crate::gui) preview_frame: Option<u64>,
    pub(in crate::gui) camera_frames: Vec<(String, u64)>,
    /// Whether the old rule, waiting for pending commands, would have held
    /// this frame back.
    pub(in crate::gui) held_by_pending: bool,
    /// When a wake to stage the next frame is due, once one is asked for.
    pub(in crate::gui) wake: Option<Instant>,
}

impl VideoSink {
    /// Open `<stem>-timing.jsonl` beside `base` and start the writers.
    pub(in crate::gui) fn open(base: &Path) -> anyhow::Result<Self> {
        Self::with_writers(base, WRITERS)
    }

    pub(in crate::gui) fn with_writers(base: &Path, writers: usize) -> anyhow::Result<Self> {
        let stem = base.file_stem().unwrap_or_default().to_string_lossy();
        let path = base.with_file_name(format!("{stem}-timing.jsonl"));
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let timing = File::options().create_new(true).append(true).open(&path)?;
        let (jobs, queue) = mpsc::sync_channel::<Job>(QUEUE);
        let (report, results) = mpsc::channel();
        let queue = Arc::new(Mutex::new(queue));
        let writers = (0..writers)
            .map(|_| {
                let queue = queue.clone();
                let report = report.clone();
                std::thread::spawn(move || {
                    loop {
                        let job = queue.lock().unwrap_or_else(|e| e.into_inner()).recv();
                        let Ok((_, shot, path)) = job else { break };
                        let result =
                            super::save_screenshot_with(&shot, &path, png::Compression::Fast)
                                .map_err(|error| format!("{error:#}"));
                        if report.send(result).is_err() {
                            break;
                        }
                    }
                })
            })
            .collect();
        Ok(Self {
            timing,
            jobs: Some(jobs),
            _queue: queue,
            results,
            writers,
            backlog: None,
            draining: false,
            written: 0,
            staged_at: None,
            staged_draws: 0,
            first_staged: None,
            preview_frame: None,
            camera_frames: Vec::new(),
            held_by_pending: false,
            wake: None,
        })
    }

    /// Note what a frame shows as it is staged.
    pub(in crate::gui) fn stage(
        &mut self,
        now: Instant,
        draws: u64,
        preview_frame: Option<u64>,
        camera_frames: Vec<(String, u64)>,
        held_by_pending: bool,
    ) {
        self.staged_at = Some(now);
        self.first_staged.get_or_insert(now);
        self.staged_draws = draws;
        self.preview_frame = preview_frame;
        self.camera_frames = camera_frames;
        self.held_by_pending = held_by_pending;
    }

    /// Append frame `index`'s timing line, as its pixels arrive.
    pub(in crate::gui) fn record(&mut self, index: u32, path: &Path) -> std::io::Result<()> {
        let origin = self.first_staged.unwrap_or_else(Instant::now);
        let ms = |at: Instant| at.saturating_duration_since(origin).as_secs_f64() * 1000.0;
        let file = path.file_name().unwrap_or_default().to_string_lossy();
        let mut text = line(
            index,
            &file,
            ms(self.staged_at.unwrap_or(origin)),
            ms(Instant::now()),
            self.preview_frame,
            &self.camera_frames,
            self.held_by_pending,
        );
        text.push('\n');
        self.timing.write_all(text.as_bytes())
    }

    /// Hand `job` to the writers; a full queue keeps it as the backlog.
    /// Whether it was accepted.
    pub(in crate::gui) fn send(&mut self, job: Job) -> Result<bool, String> {
        let Some(jobs) = &self.jobs else {
            return Err("screenshot writers already closed".into());
        };
        match jobs.try_send(job) {
            Ok(()) => Ok(true),
            Err(TrySendError::Full(job)) => {
                self.backlog = Some(job);
                Ok(false)
            }
            Err(TrySendError::Disconnected(_)) => Err("screenshot writers stopped".into()),
        }
    }

    /// Retry the backlog; whether it was accepted (true with none left).
    pub(in crate::gui) fn retry(&mut self) -> Result<bool, String> {
        match self.backlog.take() {
            Some(job) => self.send(job),
            None => Ok(true),
        }
    }

    /// Collect finished writes; the first failure, if any.
    pub(in crate::gui) fn poll(&mut self) -> Result<u32, String> {
        while let Ok(result) = self.results.try_recv() {
            result?;
            self.written += 1;
        }
        Ok(self.written)
    }

    /// Frames written so far, as of the last `poll`.
    pub(in crate::gui) fn written(&self) -> u32 {
        self.written
    }

    /// Stop the writers once they finish; how many frames were written.
    pub(in crate::gui) fn close(mut self) -> Result<u32, String> {
        self.jobs = None;
        self.backlog = None;
        for writer in std::mem::take(&mut self.writers) {
            writer
                .join()
                .map_err(|_| "screenshot writer panicked".to_string())?;
        }
        self.poll()
    }
}

/// One sidecar line. `file` is the bare file name, relative to the frame
/// directory; times are milliseconds since the first frame was staged.
pub(in crate::gui) fn line(
    index: u32,
    file: &str,
    staged_ms: f64,
    read_ms: f64,
    preview_frame: Option<u64>,
    camera_frames: &[(String, u64)],
    held_by_pending: bool,
) -> String {
    let round = |ms: f64| (ms * 1000.0).round() / 1000.0;
    let cameras: serde_json::Map<String, serde_json::Value> = camera_frames
        .iter()
        .map(|(id, frames)| (id.clone(), (*frames).into()))
        .collect();
    serde_json::json!({
        "index": index,
        "file": file,
        "staged_ms": round(staged_ms),
        "read_ms": round(read_ms),
        "preview_frame": preview_frame,
        "camera_frames": cameras,
        "held_by_pending": u32::from(held_by_pending),
    })
    .to_string()
}
