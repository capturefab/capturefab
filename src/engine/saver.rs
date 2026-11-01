//! Shots are encoded and written by a pool of encoder threads, so a slow
//! encoder or disk never holds up acquisition on the camera thread. Frames
//! wait for an encoder in a queue bounded by their bytes: a shot that would
//! overflow it is not taken, and its job counts the frame dropped. Files are
//! published as soon as they are written, and reported in the order their
//! frames came, so a job's `saved` lines, manifest lines and files keep the
//! frames' order.
use super::*;
use std::sync::mpsc::Sender;

/// The most frame bytes the encoders hold, waiting and being encoded: a
/// burst of large frames keeps up to a gigabyte of them in memory.
pub(super) const QUEUE_BYTES: usize = 1 << 30;

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
    pub(super) now: chrono::DateTime<chrono::FixedOffset>,
    pub(super) index: u32,
    pub(super) camera: crate::naming::CameraName,
    pub(super) plan: crate::naming::Plan,
    pub(super) on_exists: crate::naming::OnExists,
    pub(super) format: String,
    /// JPEG quality, when not the default.
    pub(super) quality: Option<u8>,
    pub(super) sync: crate::storage::SyncMode,
    /// The job's manifest, which gets a line once the file is published.
    pub(super) manifest: Option<Arc<super::saved::Manifest>>,
    pub(super) source: Source,
    pub(super) context: crate::context::Context,
    /// Measure the frame and preview it, as inspect does: the job's last.
    pub(super) inspect: bool,
}

/// A job's saved file, or why it was not saved.
pub(super) struct Saved {
    pub(super) job: u64,
    /// The number of the job's frame it was.
    pub(super) index: u32,
    pub(super) result: Result<SavedShot>,
}

/// A file a shot was saved to, with what its job reports of the frame.
pub(super) struct SavedShot {
    pub(super) path: String,
    pub(super) frame: crate::scheduling::JobFrame,
    /// The frame's block ID and the camera's clock when it was taken.
    pub(super) id: u64,
    pub(super) device_ts_ns: Option<u64>,
    pub(super) wall_ms: u64,
    pub(super) settings: serde_json::Map<String, Value>,
    /// Its measurements and preview, when asked for.
    pub(super) inspection: Option<Value>,
}

/// A published file, waiting for its turn to be reported.
pub(super) struct Written {
    pub(super) file: super::saved::SavedFile,
    /// Its manifest line, appended when it is reported.
    pub(super) line: Option<(Arc<super::saved::Manifest>, Value)>,
    pub(super) shot: SavedShot,
}

type Write = fn(&WorkerHandle, &Shot, &Save) -> Result<Written>;

/// The shots waiting for an encoder.
#[derive(Default)]
struct Queue {
    state: Mutex<Waiting>,
    ready: Condvar,
}
#[derive(Default)]
struct Waiting {
    /// Shots by their number, in the order their frames came.
    shots: VecDeque<(u64, Shot)>,
    /// Bytes of the frames waiting and being encoded.
    bytes: usize,
    /// Shots handed over and not reported yet.
    held: usize,
    closed: bool,
}
impl Queue {
    fn lock(&self) -> std::sync::MutexGuard<'_, Waiting> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Encoded shots waiting to be reported in order.
#[derive(Default)]
struct Order {
    /// The number of the next shot to report.
    next: u64,
    /// What became of each save of the shots encoded ahead of it: its job,
    /// the job's frame number and its file.
    done: BTreeMap<u64, Vec<(u64, u32, Result<Written>)>>,
}

pub(super) struct Saver {
    queue: Arc<Queue>,
    order: Arc<Mutex<Order>>,
    /// The most frame bytes it holds.
    capacity: usize,
    /// The number of the next shot handed over.
    next: u64,
    pub(super) saved: Receiver<Saved>,
    outbox: Sender<Saved>,
    h: WorkerHandle,
    write: Write,
    /// Most cameras never run a capture job, so the encoders start with the
    /// first shot.
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl Saver {
    pub(super) fn new(h: WorkerHandle) -> Self {
        Self::with(h, write, QUEUE_BYTES)
    }

    pub(super) fn with(h: WorkerHandle, write: Write, capacity: usize) -> Self {
        let (outbox, saved) = mpsc::channel();
        Self {
            queue: Arc::default(),
            order: Arc::default(),
            capacity,
            next: 0,
            saved,
            outbox,
            h,
            write,
            threads: Vec::new(),
        }
    }

    /// Whether a shot of a `bytes` frame handed over now is taken. One
    /// frame is always taken, however large.
    pub(super) fn has_room(&self, bytes: usize) -> bool {
        let state = self.queue.lock();
        state.bytes == 0 || state.bytes.saturating_add(bytes) <= self.capacity
    }

    /// Shots handed over and not reported yet.
    #[cfg(test)]
    pub(super) fn held(&self) -> usize {
        self.queue.lock().held
    }

    /// Hand a shot over; callers check `has_room` first.
    pub(super) fn offer(&mut self, shot: Shot) {
        if self.threads.is_empty() {
            for index in 0..crate::types::processing_threads(usize::MAX) {
                let encoder = Encoder {
                    queue: self.queue.clone(),
                    order: self.order.clone(),
                    outbox: self.outbox.clone(),
                    h: self.h.clone(),
                    write: self.write,
                };
                let thread = std::thread::Builder::new()
                    .name(format!("capturefab-encoder-{index}"))
                    .spawn(move || encoder.run());
                match thread {
                    Ok(thread) => self.threads.push(thread),
                    // Fewer encoders are slower, not wrong; with none the
                    // shot fails below.
                    Err(error) => self.h.log("warn", format!("encoder thread: {error}")),
                }
            }
        }
        if self.threads.is_empty() {
            for save in shot.saves {
                let _ = self.outbox.send(Saved {
                    job: save.job,
                    index: save.index,
                    result: Err(anyhow::anyhow!("no encoder thread could start")),
                });
            }
            return;
        }
        let mut state = self.queue.lock();
        state.bytes = state.bytes.saturating_add(shot.frame.data.len());
        state.held += 1;
        state.shots.push_back((self.next, shot));
        self.next += 1;
        drop(state);
        self.queue.ready.notify_one();
    }

    /// Take back job `job`'s shots no encoder has started, so a stopped job
    /// saves no files after its end; they end as interrupted.
    pub(super) fn discard(&mut self, job: u64) {
        let mut state = self.queue.lock();
        let mut taken = Vec::new();
        let mut emptied = Vec::new();
        state.shots.retain_mut(|(number, shot)| {
            shot.saves.retain(|save| {
                if save.job == job {
                    taken.push(save.index);
                }
                save.job != job
            });
            if shot.saves.is_empty() {
                emptied.push((*number, shot.frame.data.len()));
                return false;
            }
            true
        });
        for (_, bytes) in &emptied {
            state.bytes = state.bytes.saturating_sub(*bytes);
        }
        drop(state);
        for index in taken {
            let _ = self.outbox.send(Saved {
                job,
                index,
                result: Err(interrupted(
                    "the capture was stopped before this frame was saved",
                )),
            });
        }
        for (number, _) in emptied {
            report(
                &self.order,
                &self.queue,
                &self.h,
                &self.outbox,
                number,
                Vec::new(),
            );
        }
    }

    /// Finish the shots already handed over, and hand back what became of
    /// them.
    pub(super) fn join(mut self) -> Receiver<Saved> {
        self.close();
        std::mem::replace(&mut self.saved, mpsc::channel().1)
    }

    fn close(&mut self) {
        self.queue.lock().closed = true;
        self.queue.ready.notify_all();
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

impl Drop for Saver {
    /// Finish the shots already handed over.
    fn drop(&mut self) {
        self.close();
    }
}

/// One encoder thread of a saver.
struct Encoder {
    queue: Arc<Queue>,
    order: Arc<Mutex<Order>>,
    outbox: Sender<Saved>,
    h: WorkerHandle,
    write: Write,
}
impl Encoder {
    fn run(self) {
        loop {
            let (number, shot) = {
                let mut state = self.queue.lock();
                loop {
                    if let Some(next) = state.shots.pop_front() {
                        break next;
                    }
                    if state.closed {
                        return;
                    }
                    state = self
                        .queue
                        .ready
                        .wait(state)
                        .unwrap_or_else(|e| e.into_inner());
                }
            };
            let bytes = shot.frame.data.len();
            let results = shot
                .saves
                .iter()
                .map(|save| (save.job, save.index, (self.write)(&self.h, &shot, save)))
                .collect::<Vec<_>>();
            drop(shot);
            {
                let mut state = self.queue.lock();
                state.bytes = state.bytes.saturating_sub(bytes);
            }
            report(
                &self.order,
                &self.queue,
                &self.h,
                &self.outbox,
                number,
                results,
            );
            self.h.wake();
        }
    }
}

/// Report shot `number`'s files once every shot before it is reported: its
/// `saved` lines, then its manifest lines, then what became of each file to
/// its job.
fn report(
    order: &Mutex<Order>,
    queue: &Queue,
    h: &WorkerHandle,
    outbox: &Sender<Saved>,
    number: u64,
    results: Vec<(u64, u32, Result<Written>)>,
) {
    let mut order = order.lock().unwrap_or_else(|e| e.into_inner());
    order.done.insert(number, results);
    loop {
        let next = order.next;
        let Some(results) = order.done.remove(&next) else {
            return;
        };
        for (job, index, result) in results {
            let result = result.and_then(|written| {
                // The file is saved either way: it is reported before a
                // manifest failure ends its job.
                h.push_saved(Some(job), written.file);
                if let Some((manifest, line)) = written.line {
                    manifest.append(&line)?;
                }
                Ok(written.shot)
            });
            let _ = outbox.send(Saved { job, index, result });
        }
        order.next += 1;
        let mut state = queue.lock();
        state.held = state.held.saturating_sub(1);
    }
}

/// Encode and publish one job's file for a shot. Returns the file and its
/// manifest line, for its turn to be reported.
fn write(h: &WorkerHandle, shot: &Shot, save: &Save) -> Result<Written> {
    let meta = still_meta(&shot.frame, shot.wall_ms, &shot.acquisition, &save.context);
    let bytes = encode_still(&shot.frame, &save.format, save.quality, Some(&meta))?;
    let input = crate::naming::NameInput {
        camera: &save.camera,
        now: save.now,
        index: save.index,
        context: &save.context,
    };
    let (path, stored) = crate::naming::save_named(
        &h.names,
        &save.plan,
        &input,
        save.on_exists,
        save.path.clone(),
        |path| crate::storage::save(&bytes, path, save.sync, save.manifest.is_some()),
    )
    .with_context(|| format!("save {}", save.path.display()))?;
    if save.format == "raw" {
        save_sidecar(
            &path,
            &shot.frame,
            shot.wall_ms,
            &shot.acquisition,
            save.sync,
        )?;
    }
    let file = image_file(
        &path,
        &stored,
        &shot.frame,
        &save.format,
        shot.wall_ms,
        Some(save.job),
        &save.context,
    );
    // Measured here, from the frame in memory, beside the file.
    let inspection = save
        .inspect
        .then(|| {
            inspector::measurements(
                &shot.frame,
                save.source.kind,
                &save.source.id,
                None,
                crate::inspect::default_edge(),
                Default::default(),
                None,
            )
        })
        .transpose()
        .unwrap_or_else(|error| Some(json!({ "notes": [format!("not measured: {error:#}")] })));
    let line = save.manifest.as_ref().map(|manifest| {
        let settings = Value::Object(shot.acquisition.settings.clone());
        (
            manifest.clone(),
            file.manifest_line(&save.source.id, &settings),
        )
    });
    Ok(Written {
        file,
        line,
        shot: SavedShot {
            path: path.to_string_lossy().into_owned(),
            frame: crate::scheduling::JobFrame {
                width: shot.frame.width,
                height: shot.frame.height,
                pixel_format: shot.frame.pixel_format,
            },
            id: shot.frame.id,
            device_ts_ns: shot.frame.device_ts_ns,
            wall_ms: shot.wall_ms,
            settings: shot.acquisition.settings.clone(),
            inspection,
        },
    })
}
