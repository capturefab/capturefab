//! Scheduled shots are encoded and written on their own thread, so a slow
//! encoder or disk never holds up acquisition on the camera thread.
use super::*;
use std::sync::atomic::AtomicUsize;

/// The most frames the saver holds: one being saved and one waiting.
const FRAMES: usize = 2;

/// A copy of one frame for every job that was due when it arrived.
pub(super) struct Shot {
    pub(super) frame: Frame,
    /// When the frame arrived, UTC Unix milliseconds.
    pub(super) wall_ms: u64,
    /// The camera's settings when the frame arrived, written into the files.
    pub(super) acquisition: Acquisition,
    pub(super) saves: Vec<Save>,
}

/// One job's file for a shot, named on the camera thread.
pub(super) struct Save {
    pub(super) job: u64,
    pub(super) path: PathBuf,
    pub(super) now: chrono::DateTime<chrono::Local>,
    pub(super) index: u32,
    pub(super) camera: String,
    pub(super) plan: crate::naming::Plan,
    pub(super) on_exists: crate::naming::OnExists,
    pub(super) format: String,
    pub(super) resolved: crate::destination::Resolved,
    pub(super) storage: crate::storage::StoragePolicy,
    pub(super) context: crate::context::Context,
}

/// A job's saved file, or why it was not saved.
pub(super) struct Saved {
    pub(super) job: u64,
    pub(super) result: Result<String>,
}

type Write = fn(&WorkerHandle, &Shot, &Save) -> Result<String>;

pub(super) struct Saver {
    shots: Option<SyncSender<Shot>>,
    pub(super) saved: Receiver<Saved>,
    /// Shots handed over and not finished yet.
    held: Arc<AtomicUsize>,
    /// What the thread starts with. Most cameras never run a schedule, so it
    /// starts with the first shot.
    idle: Option<(WorkerHandle, Write, Receiver<Shot>, mpsc::Sender<Saved>)>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Saver {
    pub(super) fn new(h: WorkerHandle) -> Self {
        Self::with(h, write)
    }

    pub(super) fn with(h: WorkerHandle, write: Write) -> Self {
        let (shots, inbox) = mpsc::sync_channel::<Shot>(FRAMES);
        let (outbox, saved) = mpsc::channel();
        Self {
            shots: Some(shots),
            saved,
            held: Arc::default(),
            idle: Some((h, write, inbox, outbox)),
            thread: None,
        }
    }

    /// Whether a shot handed over now is taken without waiting.
    pub(super) fn has_room(&self) -> bool {
        self.held.load(Ordering::Acquire) < FRAMES
    }

    /// Hand a shot over. It waits only when the saver is full, which callers
    /// that must not wait rule out with `has_room`.
    pub(super) fn offer(&mut self, shot: Shot) {
        if let Some((h, write, inbox, outbox)) = self.idle.take() {
            let done = self.held.clone();
            let thread = std::thread::Builder::new()
                .name("capturefab-saver".into())
                .spawn(move || {
                    for shot in inbox {
                        for save in &shot.saves {
                            let result = write(&h, &shot, save);
                            let _ = outbox.send(Saved {
                                job: save.job,
                                result,
                            });
                        }
                        done.fetch_sub(1, Ordering::AcqRel);
                        h.wake();
                    }
                })
                .expect("spawn capture saver");
            self.thread = Some(thread);
        }
        self.held.fetch_add(1, Ordering::AcqRel);
        if let Some(shots) = &self.shots {
            // A saver thread that died disconnects `saved`, which fails the
            // jobs.
            let _ = shots.send(shot);
        }
    }
}

impl Saver {
    /// Finish the shots already handed over, and hand back what became of
    /// them.
    pub(super) fn join(mut self) -> Receiver<Saved> {
        self.shots.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        std::mem::replace(&mut self.saved, mpsc::channel().1)
    }
}

impl Drop for Saver {
    /// Finish the shots already handed over.
    fn drop(&mut self) {
        self.shots.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Encode and write one job's file for a shot, then report it. Returns the
/// path it was saved at.
fn write(h: &WorkerHandle, shot: &Shot, save: &Save) -> Result<String> {
    let meta = still_meta(&shot.frame, shot.wall_ms, &shot.acquisition, &save.context);
    let bytes = crate::frame::encode_with_meta(&shot.frame, &save.format, Some(&meta))?;
    let input = crate::naming::NameInput {
        camera: &save.camera,
        now: save.now,
        index: save.index,
    };
    let (path, stored) = crate::naming::save_named(
        &h.names,
        &save.plan,
        &input,
        save.on_exists,
        save.path.clone(),
        |path| {
            save.resolved.contain(path)?;
            crate::storage::save_bytes(&bytes, path, &save.storage)
        },
    )
    .with_context(|| format!("save {}", save.path.display()))?;
    report_image(
        h,
        &save.resolved,
        &path,
        stored,
        &shot.frame,
        &save.format,
        shot.wall_ms,
        Some(save.job),
        &save.context,
    );
    Ok(path.to_string_lossy().into_owned())
}
